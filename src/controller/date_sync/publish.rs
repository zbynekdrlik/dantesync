//! dantesync#117 / #88 / #119 — what the phase lock and the fleet date offset publish in `/status`
//! (and so in the 31900 extension, which the time server builds from the same snapshot).

use super::*;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #117 / #88 — publish the discipline, the phase lock and the fleet date offset. `D` in
    /// effect and a scheduled step are written together (one status write), so the 31900
    /// extension never counts a step twice; nothing is published while a re-anchor is pending (`D`
    /// would still be in the OLD time base) — and the time base is named by the ANCHOR's
    /// grandmaster, never the one merely heard.
    pub(in crate::controller) fn publish_date_status(&self, status: &mut SyncStatus) {
        let ds = &self.date_sync;
        status.clock_discipline = if ds.enabled {
            CLOCK_DISCIPLINE_PTP_PHASE_LOCK
        } else {
            CLOCK_DISCIPLINE_LEGACY
        }
        .to_string();
        status.rate_source = if self.phase_slew.is_some() {
            "ptp+ntp"
        } else {
            "ptp"
        }
        .to_string();
        status.ptp_phase_locked = ds.core.engaged();
        status.ptp_phase_error_us = if ds.enabled {
            ds.core.last_error_ns().map(|e| e as f64 / 1_000.0)
        } else {
            None
        };

        let now_wall = wall_now_ns();
        // #119: D IN EFFECT — the anchor plus a held slew's displacement.
        let anchor = if ds.enabled && !ds.core.rebase_pending() {
            ds.d_in_effect(now_wall)
        } else {
            None
        };
        status.date_offset_ns = anchor;
        status.date_offset_gm_uuid = anchor.and(ds.anchor_gm);
        status.date_authority = match anchor {
            None => String::new(),
            Some(_) if ds.authority.is_some() => "master".to_string(),
            Some(_) if ds.follower.adopted() => "follower".to_string(),
            Some(_) => "local".to_string(),
        };
        let published = match ds.authority.as_ref() {
            Some(a) => Some(a.announce()),
            None => ds.last_announce,
        };
        status.date_offset_seq = anchor.and(published.map(|p| p.seq));
        status.date_offset_effective_ptp_ns = anchor.and(published.map(|p| p.effective_ptp_ns));
        // #119: a slew is published by its own fields (never as a pending step), and this box's
        // own progress through the slew it follows.
        let published_slew = anchor.and(published.and_then(|p| p.as_slew()));
        status.date_slew_from_ns = published_slew.map(|s| s.from_ns);
        status.date_slew_to_ns = published_slew.map(|s| s.to_ns);
        status.date_slew_ppm = published_slew.map(|s| s.ppm);
        let base = ds.core.anchor_ns();
        status.date_slew_active =
            ds.enabled && base.is_some_and(|b| ds.follower.slew_rate_ppm(b, now_wall) != 0.0);
        status.date_slew_remaining_ms = if ds.enabled {
            base.and_then(|b| ds.follower.slew_remaining_ns(b, now_wall))
                .map(|n| n as f64 / 1e6)
        } else {
            None
        };
        // The master publishes exactly its authority's announce: D in effect on its own wall
        // (the anchor) plus the difference to the announced D. While a step is pending that is
        // the step; once its instant has passed but before this loop applies it, or while the
        // master is off the fleet line (its own PTP outage, a failed step), it is the correction
        // back to the fleet D — so a follower always reads the FLEET D, never the master's own.
        match (ds.authority.as_ref(), anchor) {
            (Some(_), Some(_)) if published_slew.is_some() => {
                status.date_step_pending_ns = None;
                status.date_step_due_in_ms = None;
            }
            (Some(a), Some(d)) => {
                let ann = a.announce();
                let delta = ann.date_offset_ns.wrapping_sub(d);
                status.date_step_pending_ns = (delta != 0).then_some(delta);
                status.date_step_due_in_ms = status.date_step_pending_ns.map(|_| {
                    ann.effective_ptp_ns.wrapping_sub(now_wall.wrapping_sub(d)) / 1_000_000
                });
            }
            _ => {
                status.date_step_pending_ns = ds.follower.pending().map(|p| p.delta_ns);
                status.date_step_due_in_ms =
                    ds.follower.time_to_due_ns(now_wall).map(|n| n / 1_000_000);
            }
        }
        // #119 follow-up: the micro-correction state.
        status.date_offset_micro = anchor.is_some() && published.is_some_and(|p| p.micro);
        status.date_micro_active =
            ds.enabled && base.is_some_and(|b| ds.follower.micro_in_flight(b, now_wall));
        status.date_micro_last_us = ds.last_micro_ns.map(|n| n / 1_000);
        status.date_correction_rate_ms_per_min = match (ds.authority.as_ref(), anchor) {
            (Some(a), Some(d)) => a
                .micro()
                .correction_rate_ns_per_min(now_wall.wrapping_sub(d))
                .map(|r| r / 1e6),
            _ => None,
        };
        status.date_micro_paused = match (ds.authority.as_ref(), anchor) {
            (Some(a), Some(d)) => a.micro().paused(now_wall.wrapping_sub(d)),
            _ => false,
        };
        status.date_correction_falling_behind = ds
            .authority
            .as_ref()
            .is_some_and(|a| a.micro().falling_behind());
        let master = ds.authority.is_some();
        status.date_offset_error_ms = if master {
            ds.master_utc_error_ns.map(|e| e as f64 / 1e6)
        } else {
            None
        };
        status.date_step_bound_ms = if master {
            Some(ds.step_bound_ns as f64 / 1e6)
        } else {
            None
        };
        status.last_date_step_ns = ds.last_step.map(|s| s.0);
        status.last_date_step_ts = ds.last_step.map(|s| s.1);
        status.last_date_step_kind = ds.last_step.map(|s| s.2.to_string()).unwrap_or_default();
        status.date_steps_late = ds.follower.late_steps();
        status.date_step_phase_jump_us = None;
    }
}
