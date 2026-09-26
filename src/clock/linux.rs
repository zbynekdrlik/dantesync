use super::step::{self, ClockReading, StepLead, StepOps};
use super::SystemClock;
use anyhow::{anyhow, Result};
use libc::{self, adjtimex, timespec, timex, ADJ_FREQUENCY, CLOCK_MONOTONIC, CLOCK_REALTIME};
use std::mem;
use std::time::Duration;

pub struct LinuxClock {
    original_freq: i64,
    /// dantesync#119: the learned read->set latency of a step (`super::step`).
    step_lead: StepLead,
}

impl LinuxClock {
    pub fn new() -> Result<Self> {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = 0; // Query mode

        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
            return Err(anyhow!("adjtimex failed (are you root?)"));
        }

        Ok(LinuxClock {
            original_freq: tx.freq,
            step_lead: StepLead::default(),
        })
    }
}

/// dantesync#119 (1.11.1) -- ns since the epoch as a `timespec` (the nanoseconds always in
/// `0..1e9`, also before the epoch).
fn ns_to_timespec(ns: i64) -> (i64, i64) {
    (ns.div_euclid(1_000_000_000), ns.rem_euclid(1_000_000_000))
}

fn read_clock(clock: libc::clockid_t) -> i64 {
    let mut ts: timespec = unsafe { mem::zeroed() };
    // clock_gettime cannot fail for these two always-present clocks with a valid pointer.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// dantesync#119 (1.11.1) -- Linux under the step law (`super::step`): `CLOCK_REALTIME` is the
/// wall (no coarser path is in use, so `coarse == precise`), `CLOCK_MONOTONIC` the reference: it
/// follows the frequency word like the wall and no step moves it.
struct LinuxStepOps;

impl StepOps for LinuxStepOps {
    fn read(&mut self) -> ClockReading {
        let reference = read_clock(CLOCK_MONOTONIC);
        let precise = read_clock(CLOCK_REALTIME);
        ClockReading {
            coarse_ns: precise,
            precise_ns: precise,
            reference_ns: reference,
        }
    }

    fn set(&mut self, target_ns: i64) -> std::result::Result<(), String> {
        let (sec, nsec) = ns_to_timespec(target_ns);
        let mut ts: timespec = unsafe { mem::zeroed() };
        ts.tv_sec = sec as libc::time_t;
        ts.tv_nsec = nsec as _;
        let ret = unsafe { libc::clock_settime(CLOCK_REALTIME, &ts) };
        if ret < 0 {
            return Err(format!(
                "clock_settime failed: errno={}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

impl SystemClock for LinuxClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let ppm = (factor - 1.0) * 1_000_000.0;
        let freq_val = (ppm * 65536.0) as i64;

        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = freq_val;

        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
            return Err(anyhow!("adjtimex failed to set frequency"));
        }

        Ok(())
    }

    /// dantesync#119 (1.11.1): the step law (`super::step::step_wall`) -- the wall moves by
    /// exactly `offset`, measured against `CLOCK_MONOTONIC` (the same law as on Windows).
    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let magnitude = i64::try_from(offset.as_nanos())
            .map_err(|_| anyhow!("a clock step of {:?} does not fit the time axis", offset))?;
        let requested = if sign > 0 { magnitude } else { -magnitude };
        let out = step::step_wall(&mut LinuxStepOps, &mut self.step_lead, requested)
            .map_err(|e| anyhow!(e))?;
        super::log_step_outcome(&out, self.step_lead);
        Ok(())
    }
}

impl Drop for LinuxClock {
    fn drop(&mut self) {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = self.original_freq;
        unsafe { adjtimex(&mut tx) };
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    /// Test PPM to freq_val conversion math
    /// The kernel uses freq_val = ppm * 65536 (16-bit fixed point)
    #[test]
    fn test_ppm_to_freq_val_conversion() {
        // Helper to compute freq_val from factor (same logic as adjust_frequency)
        fn factor_to_freq_val(factor: f64) -> i64 {
            let ppm = (factor - 1.0) * 1_000_000.0;
            (ppm * 65536.0) as i64
        }

        // No adjustment: factor = 1.0 → ppm = 0 → freq_val = 0
        assert_eq!(factor_to_freq_val(1.0), 0);

        // +100ppm: factor = 1.0001 → ppm = 100 → freq_val ≈ 6553600
        // Allow ±1 for floating point rounding
        let freq_100ppm = factor_to_freq_val(1.0001);
        assert!(
            (freq_100ppm - 6553600).abs() <= 1,
            "Expected ~6553600, got {}",
            freq_100ppm
        );

        // -100ppm: factor = 0.9999 → ppm = -100 → freq_val ≈ -6553600
        let freq_neg100ppm = factor_to_freq_val(0.9999);
        assert!(
            (freq_neg100ppm + 6553600).abs() <= 1,
            "Expected ~-6553600, got {}",
            freq_neg100ppm
        );

        // Test exact integer PPM values using direct calculation
        // +1ppm exactly: ppm * 65536 = 65536
        let freq_1ppm_direct = (1.0_f64 * 65536.0) as i64;
        assert_eq!(freq_1ppm_direct, 65536);

        // -1ppm exactly
        let freq_neg1ppm_direct = (-1.0_f64 * 65536.0) as i64;
        assert_eq!(freq_neg1ppm_direct, -65536);

        // Verify the conversion formula is correct for boundary values
        // At 500ppm (max adjustment): freq_val = 500 * 65536 = 32768000
        let freq_500ppm = (500.0_f64 * 65536.0) as i64;
        assert_eq!(freq_500ppm, 32768000);
    }

    /// dantesync#119: a step target as a `timespec`, nanoseconds normalized also before the epoch.
    #[test]
    fn a_step_target_is_a_normalized_timespec_119() {
        use super::ns_to_timespec;
        assert_eq!(
            ns_to_timespec(1_790_380_800_123_456_789),
            (1_790_380_800, 123_456_789)
        );
        assert_eq!(ns_to_timespec(1_500_000_000), (1, 500_000_000));
        assert_eq!(ns_to_timespec(0), (0, 0));
        assert_eq!(ns_to_timespec(-1), (-1, 999_999_999));
    }

    /// dantesync#119: the step law's two Linux clocks advance together while nothing steps (no
    /// root needed to read them): a mix-up of the clocks or of the units would be off by orders of
    /// magnitude.
    #[test]
    fn the_step_references_advance_together_on_linux_119() {
        use super::super::step::StepOps;
        let mut ops = super::LinuxStepOps;
        let a = ops.read();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let b = ops.read();
        let d_precise = b.precise_ns - a.precise_ns;
        let d_reference = b.reference_ns - a.reference_ns;
        assert!(
            (190_000_000..1_000_000_000).contains(&d_reference),
            "the reference advanced {d_reference} ns in a 200 ms sleep"
        );
        assert!(
            (d_precise - d_reference).abs() < 100_000,
            "precise {d_precise} ns vs reference {d_reference} ns"
        );
        assert_eq!(b.coarse_ns, b.precise_ns);
    }
}
