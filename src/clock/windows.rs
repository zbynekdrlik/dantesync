//! Windows clock control using SetSystemTimeAdjustmentPrecise (64-bit Precise API).
//!
//! This module includes comprehensive diagnostics to verify that frequency
//! adjustment actually affects clock speed.

use super::step::{self, ClockReading, StepLead, StepOps};
use super::SystemClock;
use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, GetSystemTimeAsFileTime, GetSystemTimePreciseAsFileTime,
    SetSystemTimeAdjustmentPrecise,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// #80: NtSetSystemTime (ntdll.dll) sets the system clock from a raw FILETIME
// (100ns-tick i64/LARGE_INTEGER) directly -- no SYSTEMTIME intermediate, hence
// no millisecond-quantization. This is undocumented (not covered by
// win32metadata, so not in the `windows` crate's generated bindings) but has
// been a stable NTDLL export since NT 3.1, and is the well-established
// mechanism every serious NTP/PTP daemon on Windows uses to step the clock
// with full precision -- the public Win32 surface has no precise SETTER
// analogous to GetSystemTimePreciseAsFileTime (Microsoft never shipped one).
// Requires SeSystemtimePrivilege, already enabled at construction (see
// `enable_privilege` below) for the legacy SetSystemTime call this replaces.
//
// Deliberately a STATIC link (`#[link(name = "ntdll")]`), unlike this codebase's own
// established pattern for an undocumented/optional Windows surface -- net_pcap.rs's
// `wpcap_runtime_available()` probes the (third-party, genuinely-optional) Npcap runtime
// dynamically via `LoadLibraryW`/`GetProcAddress` specifically so a missing DLL degrades
// gracefully instead of crashing the whole process at load time (review finding, #80).
// `ntdll.dll` is different in kind: it is core, unconditionally-loaded OS infrastructure on
// every NT-based Windows version (not optional third-party software), and `SetSystemTime`
// itself is implemented on top of the same underlying NT mechanism `NtSetSystemTime` reaches
// -- if `SetSystemTime` has ever worked on a given box, `NtSetSystemTime` necessarily works
// identically. A dynamic probe here would add complexity for a failure mode (`ntdll.dll`
// missing or this specific stable-since-NT-3.1 export vanishing) with no realistic path to
// occurring on any Windows version this project targets.
#[link(name = "ntdll")]
extern "system" {
    fn NtSetSystemTime(new_time: *const i64, old_time: *mut i64) -> i32;
}

fn filetime_u64(ft: windows::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64)
}

/// dantesync#119 (1.11.1) -- Windows under the step law (`super::step`): the coarse and the precise
/// system time, and QPC at the system time's rate as the step-immune reference.
struct WindowsStepOps {
    perf_frequency: i64,
    increment: u64,
    adjustment: u64,
}

impl WindowsStepOps {
    /// The QPC-to-system-time rate is read once per step: nothing changes the adjustment while
    /// the step runs (only this daemon writes it, from the same thread).
    fn new(perf_frequency: i64) -> Self {
        let (mut adj, mut inc, mut disabled) = (0u64, 0u64, BOOL(0));
        let read = unsafe { GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled) };
        if let Err(e) = read {
            warn!(
                "[StepClock] GetSystemTimeAdjustmentPrecise failed ({}) -- measuring the step at \
                 the nominal rate (off by at most the frequency word over the ~0.1 s call)",
                e
            );
            (adj, inc) = (1, 1);
        }
        WindowsStepOps {
            perf_frequency,
            increment: inc,
            adjustment: adj,
        }
    }
}

impl StepOps for WindowsStepOps {
    fn read(&mut self) -> ClockReading {
        let (mut qpc_before, mut qpc_after) = (0i64, 0i64);
        // QueryPerformanceCounter cannot fail since Windows XP (documented); a failed read would
        // leave 0, a reading the step law's window check and correction bound refuse to act on.
        // The coarse read before the precise one, so it can never be ahead of it.
        let (coarse, precise) = unsafe {
            let _ = QueryPerformanceCounter(&mut qpc_before);
            let coarse = filetime_u64(GetSystemTimeAsFileTime());
            let precise = filetime_u64(GetSystemTimePreciseAsFileTime());
            let _ = QueryPerformanceCounter(&mut qpc_after);
            (coarse, precise)
        };
        let reference = |qpc| {
            super::qpc_to_reference_ns(qpc, self.perf_frequency, self.increment, self.adjustment)
        };
        ClockReading::sandwiched(
            super::filetime_to_unix_ns(coarse),
            super::filetime_to_unix_ns(precise),
            reference(qpc_before),
            reference(qpc_after),
        )
    }

    fn set(&mut self, target_ns: i64) -> std::result::Result<(), String> {
        // #80: NtSetSystemTime takes the target as a raw FILETIME (100 ns ticks) -- no SYSTEMTIME
        // intermediate, so no millisecond quantization. Requires SeSystemtimePrivilege, enabled
        // at construction. NT_SUCCESS(status) is `status >= 0`.
        let new_time = super::unix_ns_to_filetime(target_ns) as i64;
        let mut previous_time: i64 = 0;
        let status = unsafe { NtSetSystemTime(&new_time, &mut previous_time) };
        if status < 0 {
            return Err(format!(
                "NtSetSystemTime failed with NTSTATUS 0x{:08X}",
                status as u32
            ));
        }
        Ok(())
    }
}

pub struct WindowsClock {
    original_increment: u64,
    perf_frequency: i64,
    /// dantesync#119: the learned read->set latency of a step (`super::step`).
    step_lead: StepLead,

    // Diagnostic tracking
    adjustment_count: u64,
    last_adjustment: u64,
    last_requested_ppm: f64,

    // High-precision measurement baseline (for diagnostics)
    baseline_perf_counter: i64,
    baseline_filetime: u64,
    last_measurement_time: Instant,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        // Get performance counter frequency
        let mut perf_freq: i64 = 0;
        unsafe {
            QueryPerformanceFrequency(&mut perf_freq)?;
        }

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled)?;
        }

        // Calculate PPM sensitivity
        let ppm_per_unit = 1_000_000.0 / inc as f64;
        debug!(
            "[Clock] Windows API initialized: PerfFreq={:.1}MHz, Sensitivity={:.6}PPM/unit",
            perf_freq as f64 / 1_000_000.0,
            ppm_per_unit
        );

        // Current PPM offset from nominal
        let current_ppm = ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0;
        debug!(
            "Current PPM offset: {:+.3} PPM (Adj {} vs Nominal {})",
            current_ppm, adj, inc
        );

        // Enable adjustment if disabled
        if disabled.as_bool() {
            warn!("Time adjustment was DISABLED! Enabling...");
            unsafe {
                SetSystemTimeAdjustmentPrecise(inc, false)?;
            }
            info!("Time adjustment ENABLED with nominal value.");
        }

        // Get baseline measurements
        let (baseline_pc, baseline_ft) = unsafe {
            let mut pc: i64 = 0;
            QueryPerformanceCounter(&mut pc)?;
            (pc, filetime_u64(GetSystemTimeAsFileTime()))
        };

        let clock = WindowsClock {
            original_increment: inc,
            perf_frequency: perf_freq,
            step_lead: StepLead::default(),
            adjustment_count: 0,
            last_adjustment: inc,
            last_requested_ppm: 0.0,
            baseline_perf_counter: baseline_pc,
            baseline_filetime: baseline_ft,
            last_measurement_time: Instant::now(),
        };

        // Check for interfering processes
        clock.check_for_interference();

        info!("Frequency adjustment API initialized (inverted sign correction applied).");

        Ok(clock)
    }

    /// Check for processes that might interfere with time adjustment
    fn check_for_interference(&self) {
        info!("");
        info!("Checking for interfering processes...");

        // Check if W32Time service is running
        let w32time_check = std::process::Command::new("sc")
            .args(["query", "w32time"])
            .output();

        match w32time_check {
            Ok(output) => {
                let output_str = String::from_utf8_lossy(&output.stdout);
                if output_str.contains("RUNNING") {
                    error!(
                        "⚠ W32Time service is RUNNING! This will interfere with time adjustment."
                    );
                    error!("  Run: net stop w32time");
                } else if output_str.contains("STOPPED") {
                    info!("✓ W32Time service is stopped.");
                } else {
                    info!(
                        "  W32Time status: {}",
                        output_str.lines().next().unwrap_or("unknown")
                    );
                }
            }
            Err(e) => {
                warn!("Could not check W32Time status: {}", e);
            }
        }

        // Check current adjustment state
        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);
        unsafe {
            if GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled).is_ok() {
                if disabled.as_bool() {
                    error!("⚠ Time adjustment is DISABLED! Another process may have disabled it.");
                } else {
                    info!("✓ Time adjustment is enabled.");
                }
            }
        }
        info!("");
    }

    fn enable_privilege(name: &str) -> Result<()> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )?;

            let mut luid = LUID::default();
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name_wide.as_ptr()), &mut luid)?;

            let mut tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                ..Default::default()
            };
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

            AdjustTokenPrivileges(token, BOOL(0), Some(&tp), 0, None, None)?;

            if let Err(e) = GetLastError() {
                if e.code() == ERROR_NOT_ALL_ASSIGNED.to_hresult() {
                    return Err(anyhow!(
                        "Failed to adjust privilege: ERROR_NOT_ALL_ASSIGNED. Run as Administrator!"
                    ));
                }
            }

            CloseHandle(token)?;
        }
        Ok(())
    }

    /// Measure current clock rate vs wall clock and log detailed diagnostics
    fn measure_and_log_effectiveness(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_measurement_time);

        // Only measure if enough time has passed (at least 1 second for accuracy)
        if elapsed.as_secs() < 1 {
            return;
        }

        unsafe {
            // Get current measurements
            let mut current_pc: i64 = 0;
            if QueryPerformanceCounter(&mut current_pc).is_err() {
                return;
            }

            let current_ft_u64 = filetime_u64(GetSystemTimeAsFileTime());

            // Calculate elapsed times
            let pc_elapsed = current_pc - self.baseline_perf_counter;
            let wall_time_ns = (pc_elapsed as f64 / self.perf_frequency as f64) * 1_000_000_000.0;

            let ft_elapsed = current_ft_u64 - self.baseline_filetime;
            let system_time_ns = ft_elapsed as f64 * 100.0;

            // Calculate observed PPM since baseline
            let time_diff_ns = system_time_ns - wall_time_ns;
            let observed_ppm = (time_diff_ns / wall_time_ns) * 1_000_000.0;

            // Calculate effectiveness
            let effectiveness = if self.last_requested_ppm.abs() > 0.1 {
                observed_ppm / self.last_requested_ppm
            } else {
                if observed_ppm.abs() < 10.0 {
                    1.0
                } else {
                    0.0
                }
            };

            // Log every 10 seconds worth of measurements (debug level - matches Linux simplicity)
            if elapsed.as_secs() >= 10 {
                debug!("[FreqMeasure] Elapsed: {:.1}s | Requested: {:+.1} PPM | Observed: {:+.1} PPM | Effectiveness: {:.0}%",
                      wall_time_ns / 1_000_000_000.0, self.last_requested_ppm, observed_ppm, effectiveness * 100.0);

                if effectiveness.abs() < 0.3 && self.last_requested_ppm.abs() > 10.0 {
                    debug!(
                        "[FreqMeasure] LOW EFFECTIVENESS! Frequency adjustment may not be working."
                    );
                }

                // Reset baseline for next measurement period
                self.baseline_perf_counter = current_pc;
                self.baseline_filetime = current_ft_u64;
                self.last_measurement_time = now;
            }
        }
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let ppm = (factor - 1.0) * 1_000_000.0;

        // Calculate adjustment delta
        // NOTE: Windows has INVERTED behavior - increasing adjustment slows the clock!
        // To speed up (positive PPM), we must DECREASE the adjustment value.
        let adjustment_delta = (-ppm * self.perf_frequency as f64 / 1_000_000.0).round() as i64;
        let new_adj = (self.original_increment as i64 + adjustment_delta) as u64;

        self.adjustment_count += 1;

        // Calculate delta for logging
        let delta_from_nominal = new_adj as i64 - self.original_increment as i64;

        // Log adjustments at debug level (matches Linux simplicity)
        debug!(
            "[FreqAdj #{}] {:+.3} PPM | Adj: {} → {} (Δ{:+} from nominal)",
            self.adjustment_count, ppm, self.last_adjustment, new_adj, delta_from_nominal
        );

        unsafe {
            // Apply adjustment
            SetSystemTimeAdjustmentPrecise(new_adj, false)?;

            // Verify
            let mut verify_adj = 0u64;
            let mut verify_inc = 0u64;
            let mut verify_disabled = BOOL(0);

            if GetSystemTimeAdjustmentPrecise(
                &mut verify_adj,
                &mut verify_inc,
                &mut verify_disabled,
            )
            .is_ok()
            {
                if verify_adj != new_adj {
                    error!(
                        "[FreqAdj] MISMATCH! Requested={}, Actual={}",
                        new_adj, verify_adj
                    );
                }
                if verify_disabled.as_bool() {
                    error!("[FreqAdj] TIME ADJUSTMENT DISABLED! Interference detected!");
                    // Try to re-enable
                    let _ = SetSystemTimeAdjustmentPrecise(new_adj, false);
                }
            }
        }

        self.last_adjustment = new_adj;
        self.last_requested_ppm = ppm;

        // Periodic effectiveness measurement
        self.measure_and_log_effectiveness();

        Ok(())
    }

    /// dantesync#119 (1.11.1): the step law (`super::step::step_wall`) -- the wall moves by
    /// exactly `offset`, measured against QPC. The read-modify-write this replaces read the
    /// COARSE system time and so landed short by the clock-interrupt lag on every step.
    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let magnitude = i64::try_from(offset.as_nanos())
            .map_err(|_| anyhow!("a clock step of {:?} does not fit the time axis", offset))?;
        let requested = if sign > 0 { magnitude } else { -magnitude };
        let mut ops = WindowsStepOps::new(self.perf_frequency);
        let out =
            step::step_wall(&mut ops, &mut self.step_lead, requested).map_err(|e| anyhow!(e))?;
        super::log_step_outcome(&out, self.step_lead);

        // Reset the frequency-effectiveness measurement baseline after the step.
        let mut pc: i64 = 0;
        unsafe {
            let _ = QueryPerformanceCounter(&mut pc);
            self.baseline_filetime = filetime_u64(GetSystemTimeAsFileTime());
        }
        self.baseline_perf_counter = pc;
        self.last_measurement_time = Instant::now();
        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        debug!(
            "[Clock] Shutdown: {} adjustments, resetting to nominal",
            self.adjustment_count
        );

        unsafe {
            match SetSystemTimeAdjustmentPrecise(self.original_increment, false) {
                Ok(_) => info!("Clock reset to nominal successfully."),
                Err(e) => error!("Failed to reset clock: {}", e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// Test PPM to frequency adjustment delta conversion
    ///
    /// Windows frequency adjustment formula:
    /// - adjustment_delta = (-ppm * perf_frequency / 1_000_000).round()
    /// - new_adj = original_increment + adjustment_delta
    ///
    /// NOTE: Sign is INVERTED - increasing adjustment SLOWS the clock!
    /// So positive PPM (speed up) requires DECREASING the adjustment.
    #[test]
    fn test_ppm_to_adjustment_delta_conversion() {
        // Typical Windows performance frequency: 10 MHz (10,000,000 Hz)
        let perf_frequency: i64 = 10_000_000;

        // Test: +10 PPM (speed up by 10 parts per million)
        // Expected: negative delta (decrease adjustment to speed up)
        let ppm = 10.0f64;
        let adjustment_delta = (-ppm * perf_frequency as f64 / 1_000_000.0).round() as i64;
        assert_eq!(adjustment_delta, -100, "+10 PPM should give delta of -100");

        // Test: -10 PPM (slow down)
        // Expected: positive delta (increase adjustment to slow down)
        let ppm = -10.0f64;
        let adjustment_delta = (-ppm * perf_frequency as f64 / 1_000_000.0).round() as i64;
        assert_eq!(adjustment_delta, 100, "-10 PPM should give delta of +100");

        // Test: 0 PPM (no change)
        let ppm = 0.0f64;
        let adjustment_delta = (-ppm * perf_frequency as f64 / 1_000_000.0).round() as i64;
        assert_eq!(adjustment_delta, 0, "0 PPM should give delta of 0");
    }

    /// Test that adjustment delta correctly modifies increment
    #[test]
    fn test_adjustment_applied_to_increment() {
        // Typical original_increment value
        let original_increment: u64 = 156_250;
        let perf_frequency: i64 = 10_000_000;

        // Apply +50 PPM (speed up)
        let ppm = 50.0f64;
        let adjustment_delta = (-ppm * perf_frequency as f64 / 1_000_000.0).round() as i64;
        let new_adj = (original_increment as i64 + adjustment_delta) as u64;

        // +50 PPM → delta = -500 → new_adj = 156_250 - 500 = 155_750
        assert_eq!(new_adj, 155_750);
        assert!(
            new_adj < original_increment,
            "Positive PPM should decrease adjustment"
        );

        // Apply -50 PPM (slow down)
        let ppm = -50.0f64;
        let adjustment_delta = (-ppm * perf_frequency as f64 / 1_000_000.0).round() as i64;
        let new_adj = (original_increment as i64 + adjustment_delta) as u64;

        // -50 PPM → delta = +500 → new_adj = 156_250 + 500 = 156_750
        assert_eq!(new_adj, 156_750);
        assert!(
            new_adj > original_increment,
            "Negative PPM should increase adjustment"
        );
    }

    /// Test factor to PPM conversion
    #[test]
    fn test_factor_to_ppm_conversion() {
        // factor = 1.0 → 0 PPM (no adjustment)
        let factor = 1.0f64;
        let ppm = (factor - 1.0) * 1_000_000.0;
        assert_eq!(ppm, 0.0);

        // factor = 1.00001 → +10 PPM (speed up by 10 ppm)
        let factor = 1.00001f64;
        let ppm = (factor - 1.0) * 1_000_000.0;
        assert!((ppm - 10.0).abs() < 0.01);

        // factor = 0.99999 → -10 PPM (slow down by 10 ppm)
        let factor = 0.99999f64;
        let ppm = (factor - 1.0) * 1_000_000.0;
        assert!((ppm - (-10.0)).abs() < 0.01);
    }

    /// Test PPM per unit sensitivity calculation
    #[test]
    fn test_ppm_sensitivity() {
        // With increment = 156_250, each unit change = 1_000_000 / 156_250 ≈ 6.4 PPM
        let inc: u64 = 156_250;
        let ppm_per_unit = 1_000_000.0 / inc as f64;
        assert!((ppm_per_unit - 6.4).abs() < 0.1);
    }

    /// Test current PPM offset calculation from adjustment
    #[test]
    fn test_current_ppm_offset_calculation() {
        let inc: u64 = 156_250; // nominal increment

        // Same as nominal → 0 PPM
        let adj = inc;
        let current_ppm = ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0;
        assert_eq!(current_ppm, 0.0);

        // adj = 156_350 → positive offset (clock running slow, higher adjustment)
        let adj = 156_350u64;
        let current_ppm = ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0;
        assert!((current_ppm - 640.0).abs() < 10.0); // ~640 PPM slow

        // adj = 156_150 → negative offset (clock running fast, lower adjustment)
        let adj = 156_150u64;
        let current_ppm = ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0;
        assert!((current_ppm - (-640.0)).abs() < 10.0); // ~-640 PPM fast
    }

    /// Test step clock offset to 100ns conversion
    #[test]
    fn test_step_offset_to_100ns() {
        use std::time::Duration;

        // 1 millisecond = 10,000 units of 100ns
        let offset = Duration::from_millis(1);
        let offset_100ns = offset.as_nanos() as u64 / 100;
        assert_eq!(offset_100ns, 10_000);

        // 1 second = 10,000,000 units of 100ns
        let offset = Duration::from_secs(1);
        let offset_100ns = offset.as_nanos() as u64 / 100;
        assert_eq!(offset_100ns, 10_000_000);

        // 1 microsecond = 10 units of 100ns
        let offset = Duration::from_micros(1);
        let offset_100ns = offset.as_nanos() as u64 / 100;
        assert_eq!(offset_100ns, 10);
    }

    /// dantesync#119: on a real Windows box (the CI leg) the step law's two clocks agree while
    /// nothing steps -- the precise system time and QPC at the system-time rate advance together.
    /// A unit slip (100 ns vs ns, a QPC frequency mix-up) would be off by orders of magnitude.
    #[test]
    fn the_step_references_advance_together_on_windows_119() {
        use super::super::step::read_tight;
        let mut freq: i64 = 0;
        unsafe {
            windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq).unwrap();
        }
        let mut ops = super::WindowsStepOps::new(freq);
        // The best of a few pairs: a shared CI runner may preempt, slew or step any single one.
        let best = (0..5)
            .map(|_| {
                let a = read_tight(&mut ops);
                std::thread::sleep(std::time::Duration::from_millis(50));
                let b = read_tight(&mut ops);
                let d_reference = b.reference_ns - a.reference_ns;
                assert!(
                    (45_000_000..1_000_000_000).contains(&d_reference),
                    "the reference advanced {d_reference} ns in a 50 ms sleep"
                );
                // The coarse clock is the precise one at the last clock interrupt: never ahead of
                // it, never more than one (default 15.6 ms) tick behind.
                assert!(
                    (0..16_000_000).contains(&(b.precise_ns - b.coarse_ns)),
                    "{b:?}"
                );
                ((b.precise_ns - a.precise_ns) - d_reference).abs()
            })
            .min()
            .unwrap();
        assert!(best < 50_000, "precise vs reference off by {best} ns");
    }
}
