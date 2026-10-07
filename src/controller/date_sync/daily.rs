//! dantesync#119 (1.12) — the controller side of the NIGHTLY date step that is not part of the
//! master's loop tick (`micro.rs` drives the authority and logs the nightly decisions): the
//! authority's startup line in daily mode, and the off-line daily master's re-join after a
//! failed step of its own. Kept apart so `date_sync.rs` stays one screenful per concern.

use super::*;

/// The NTP master's startup line when its authority corrects the date once a night.
pub(super) fn log_daily_authority(
    anchor: i64,
    authority: &DateAuthority,
    cfg: crate::date_offset::DailyConfig,
) {
    let tod_s = cfg.step_tod_ns / 1_000_000_000;
    info!(
        "[DATE] this NTP master is the fleet DATE-OFFSET AUTHORITY: D={}ns — the fleet date \
             runs at the Dante tick all day and is corrected ONCE A NIGHT: one coordinated \
             step of the UTC error rounded to whole {} ms, either direction, when the window \
             opens at {:02}:{:02}:{:02} UTC (up to {} min while UTC is unavailable), announced \
             {} s ahead; only an error beyond {} ms is stepped at once, unrounded (correction = \
             \"daily\")",
        anchor,
        cfg.step_quantum_ms(),
        tod_s / 3_600,
        tod_s % 3_600 / 60,
        tod_s % 60,
        crate::date_offset::DAILY_WINDOW_NS / 60_000_000_000,
        authority.lead_ns() * crate::date_offset::MICRO_LEAD_FACTOR / 1_000_000_000,
        cfg.emergency_ns / 1_000_000
    );
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #119 (1.12, review round 2) — a DAILY-mode master without PTP whose own D is off the fleet
    /// D (a step of its own failed) steps its wall back onto the fleet line once the failure's
    /// backoff has passed: one Join of the difference. With no PTP there is no phase error to
    /// measure and no re-alignment window ([`Self::realign_master_to_fleet`] waits for PTP), so
    /// without this it would stay a whole nightly step off the fleet until PTP returns. Nothing
    /// while a change is in flight, and nothing in micro mode (its local NTP path runs instead).
    pub(super) fn realign_offline_daily_master(&mut self) {
        if !self.date_sync.daily()
            || !self.ptp_offline
            || self.in_step_backoff()
            || self.date_sync.core.rebase_pending()
        {
            return;
        }
        let (Some(base), Some(a)) = (
            self.date_sync.core.anchor_ns(),
            self.date_sync.authority.as_ref(),
        ) else {
            return;
        };
        let now_wall = wall_now_ns();
        let own = self.date_sync.follower.in_effect_ns(base, now_wall);
        let now_ptp = now_wall.wrapping_sub(own);
        if a.pending_step_ns(now_ptp).is_some()
            || a.slew_in_progress(now_ptp).is_some()
            || self.date_sync.follower.pending().is_some()
            || self.date_sync.follower.held_slew().is_some()
        {
            return;
        }
        let delta = a.in_effect_ns(now_ptp).wrapping_sub(own);
        if delta == 0 {
            return;
        }
        let seq = a.seq();
        warn!(
            "[DATE] the off-line master is {:+}us off the fleet date offset (a failed step) — \
             stepping its OWN wall back to the fleet line",
            delta / 1_000
        );
        self.apply_date_step(delta, StepKind::Join, seq);
    }
}
