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

/// #80 -- compute the target FILETIME (100ns ticks since 1601-01-01) for a Windows clock step,
/// given the current FILETIME, the offset to apply, and its sign.
///
/// Pure arithmetic, deliberately NOT inside `windows.rs`: that module is `#[cfg(windows)]`-gated
/// at the `mod` declaration above, so it is never even PARSED on Linux CI
/// (`.claude/rules/windows-only-code.md`) -- this function lives here, unconditionally compiled,
/// so its logic (in particular the negative-time guard's exact boundary) gets real Linux-CI test
/// execution instead of being invisible until a live Windows box exercises it.
///
/// This function only computes WHAT the target should be; it does not decide HOW to apply it.
/// dantesync#80's actual bug was in the "how": `windows.rs` used to route this precise,
/// 100ns-resolution target through `FileTimeToSystemTime` + the legacy `SetSystemTime` Win32
/// API, whose `SYSTEMTIME` parameter has no field finer than whole milliseconds -- silently
/// discarding up to ~1ms of the computed target on every single step. The fix routes the SAME
/// target this function computes through the native `NtSetSystemTime` API instead (a raw
/// `LARGE_INTEGER` FILETIME, no `SYSTEMTIME` intermediate) -- this function's own contract
/// (compute the precise target) was never the defect and is unchanged by that fix.
pub fn compute_step_target_100ns(
    before_100ns: u64,
    offset: std::time::Duration,
    sign: i8,
) -> Result<u64> {
    let offset_100ns = (offset.as_nanos() / 100) as u64;
    if sign > 0 {
        // No overflow guard here (unlike the negative branch below), deliberately: a real
        // FILETIME `before_100ns` around 2026 is ~1.3e17 (100ns ticks since 1601-01-01)
        // against a `u64::MAX` of ~1.8e19 -- headroom of over 400 years even for an
        // absurdly large `offset` (review finding, #80: worth stating explicitly, since a
        // future reader has no reason to already know the FILETIME epoch is 1601).
        Ok(before_100ns + offset_100ns)
    } else if before_100ns > offset_100ns {
        Ok(before_100ns - offset_100ns)
    } else {
        Err(anyhow::anyhow!("Clock step would result in negative time"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn positive_sign_adds_the_offset() {
        assert_eq!(
            compute_step_target_100ns(1_000_000, Duration::from_micros(2_335), 1).unwrap(),
            1_000_000 + 23_350
        );
    }

    #[test]
    fn negative_sign_subtracts_the_offset_when_it_fits() {
        assert_eq!(
            compute_step_target_100ns(1_000_000, Duration::from_micros(500), -1).unwrap(),
            1_000_000 - 5_000
        );
    }

    #[test]
    fn negative_sign_exactly_equal_to_before_is_rejected() {
        // The boundary is strict (`>`, not `>=`) in the original code this was extracted from --
        // preserved exactly: a step that would land precisely on epoch-zero-relative-to-before
        // (before == offset) is treated the same as one that would go negative, not as the
        // allowed edge case.
        assert!(compute_step_target_100ns(5_000, Duration::from_nanos(500_000), -1).is_err());
    }

    #[test]
    fn negative_sign_larger_than_before_is_rejected() {
        // offset_100ns (2000, i.e. 200us) genuinely exceeds before_100ns (1000) here.
        let err = compute_step_target_100ns(1_000, Duration::from_micros(200), -1).unwrap_err();
        assert!(
            err.to_string().contains("negative"),
            "error must explain WHY the step was rejected, got: {}",
            err
        );
    }

    #[test]
    fn negative_sign_one_tick_under_before_succeeds() {
        // One 100ns tick short of the strict boundary above -- must NOT be rejected.
        assert_eq!(
            compute_step_target_100ns(5_001, Duration::from_nanos(500_000), -1).unwrap(),
            1
        );
    }

    #[test]
    fn zero_offset_returns_before_unchanged_either_sign() {
        // Review finding, #80: a genuine zero-offset call never happens in production
        // (controller.rs only steps for a measured over-threshold offset), but it costs
        // nothing to pin as a defensive edge case -- neither sign should perturb the value.
        assert_eq!(
            compute_step_target_100ns(1_000_000, Duration::ZERO, 1).unwrap(),
            1_000_000
        );
        assert_eq!(
            compute_step_target_100ns(1_000_000, Duration::ZERO, -1).unwrap(),
            1_000_000
        );
    }
}
