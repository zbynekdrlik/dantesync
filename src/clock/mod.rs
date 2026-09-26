use anyhow::Result;

#[cfg_attr(test, mockall::automock)]
pub trait SystemClock {
    /// Adjusts the system clock frequency.
    /// `factor`: The ratio of master speed to local speed.
    /// 1.0 means no adjustment.
    /// \> 1.0 means local clock is too slow, speed up.
    /// \< 1.0 means local clock is too fast, slow down.
    fn adjust_frequency(&mut self, factor: f64) -> Result<()>;

    /// Stepping the clock (for NTP initial sync)
    fn step_clock(&mut self, offset: std::time::Duration, sign: i8) -> Result<()>;
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::WindowsClock as PlatformClock;

#[cfg(unix)]
mod linux;
#[cfg(unix)]
pub use self::linux::LinuxClock as PlatformClock;

pub mod step;

/// dantesync#119 (1.11.1) -- FILETIME (100 ns ticks since 1601-01-01) of the Unix epoch.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// A Windows FILETIME as ns since the Unix epoch (the step law's time axis, `step`). Pure and
/// unconditionally compiled so Linux CI runs its tests (`windows.rs` is never parsed there).
pub fn filetime_to_unix_ns(filetime: u64) -> i64 {
    (filetime as i64 - FILETIME_UNIX_EPOCH as i64) * 100
}

/// The FILETIME for `unix_ns`, rounded to the nearest 100 ns tick (the finest `NtSetSystemTime`
/// takes).
pub fn unix_ns_to_filetime(unix_ns: i64) -> u64 {
    ((unix_ns + 50).div_euclid(100) + FILETIME_UNIX_EPOCH as i64) as u64
}

/// A QPC reading as ns at the SYSTEM TIME's rate: the kernel advances the system time by `inc`
/// per `adj` QPC counts (`GetSystemTimeAdjustmentPrecise`; a larger `adj` runs slower), so this
/// clock follows the frequency word like the wall does and never steps -- the step law's
/// reference on Windows. Only its differences are used.
pub fn qpc_to_reference_ns(qpc: i64, frequency: i64, increment: u64, adjustment: u64) -> i64 {
    let ratio = if adjustment == 0 {
        1.0
    } else {
        increment as f64 / adjustment as f64
    };
    (qpc as f64 / frequency as f64 * 1e9 * ratio).round() as i64
}

/// The one log line of a date/NTP step, both operating systems (`step::step_wall`'s outcome).
pub fn log_step_outcome(out: &step::StepOutcome, lead: step::StepLead) {
    let line = format!(
        "[StepClock] stepped {:+.1}us (requested {:+.1}us, residual {:+.1}us, {} set(s), learned \
         set latency {:.1}us, coarse clock lag {:.1}us)",
        out.realized_ns as f64 / 1e3,
        out.requested_ns as f64 / 1e3,
        out.residual_ns() as f64 / 1e3,
        out.attempts,
        lead.lead_ns() as f64 / 1e3,
        out.coarse_lag_ns as f64 / 1e3
    );
    if out.residual_ns().abs() > step::STEP_TOLERANCE_NS {
        log::warn!(
            "{} -- NOT EXACT: the phase lock pays the residual back",
            line
        );
    } else {
        log::info!("{}", line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unix_epoch_is_filetime_116444736000000000() {
        assert_eq!(filetime_to_unix_ns(FILETIME_UNIX_EPOCH), 0);
        // 2026-09-26 00:00:00 UTC = 1_790_380_800 s.
        let ft = FILETIME_UNIX_EPOCH + 1_790_380_800 * 10_000_000;
        assert_eq!(filetime_to_unix_ns(ft), 1_790_380_800_000_000_000);
    }

    #[test]
    fn a_unix_time_round_trips_through_filetime_to_the_100_ns_tick() {
        let ns: i64 = 1_790_380_800_123_456_789;
        let ft = unix_ns_to_filetime(ns);
        assert_eq!(filetime_to_unix_ns(ft), 1_790_380_800_123_456_800);
        assert_eq!(unix_ns_to_filetime(1_790_380_800_123_456_749), ft - 1);
    }

    #[test]
    fn the_qpc_reference_runs_at_the_system_time_rate() {
        // A 10 MHz QPC. Nominal: 1 s of QPC is 1 s.
        let f = 10_000_000;
        assert_eq!(qpc_to_reference_ns(f, f, 156_250, 156_250), 1_000_000_000);
        // adj 20 ppm LARGER than inc: the system time runs 20 ppm slow, and so does the reference.
        let r = qpc_to_reference_ns(f, f, 1_000_000, 1_000_020);
        assert!((r - 999_980_000).abs() <= 1, "{r}");
        // Only differences are used: a big QPC origin keeps sub-ns resolution per second.
        let origin = 3_600 * 24 * 30 * f;
        let d = qpc_to_reference_ns(origin + f, f, 1_000_000, 999_990)
            - qpc_to_reference_ns(origin, f, 1_000_000, 999_990);
        assert!((d - 1_000_010_000).abs() <= 1, "{d}");
        // A zero adjustment (never returned) is read as nominal, not a division by zero.
        assert_eq!(qpc_to_reference_ns(f, f, 156_250, 0), 1_000_000_000);
    }
}
