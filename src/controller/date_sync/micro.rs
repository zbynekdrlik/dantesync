//! dantesync#119 follow-up — the controller side of the date MICRO-corrections
//! (`crate::date_offset::MicroScheduler`): the NTP master's loop drives its authority's micro
//! clock, schedules each increment on its own wall like any announce, publishes it at once, and
//! keeps the loud `date correction falling behind` line in step with the authority's alarm. Every
//! box applies an increment through the ordinary step / slew paths (`apply_date_step`, the #119
//! slew glue), labelled `micro` and kept out of the NTP step-storm count.

use super::*;

/// While the micro-corrections fall behind, the loud line is repeated this often.
const FALLING_BEHIND_WARN_INTERVAL: Duration = Duration::from_secs(300);

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #88 / #119 — the master's own scheduler takes its authority's announce like every box
    /// (only while it is on the fleet line: off it, it re-aligns afterwards).
    pub(super) fn master_schedules_own(&mut self, ann: DateAnnounce, base: i64, now_wall: i64) {
        let act = self.date_sync.follower.on_announce(ann, base, now_wall);
        debug!("[DATE] master's own scheduler: {:?}", act);
        // Defensive — on the line D and the authority agree to the ns (the bench asserts it).
        if let FollowAction::Absorb { new_anchor_ns } = act {
            self.date_sync.core.set_anchor(new_anchor_ns);
        }
    }

    /// #119 follow-up — the NTP master's micro-correction clock, every loop iteration: the
    /// authority decides the next increment on the exact interval (not on the NTP cadence). An
    /// increment is scheduled on the master's own wall like any announce (only on the fleet line)
    /// and published at once, so followers hear it within its lead. Also keeps the loud
    /// `date correction falling behind` line in step with the authority's alarm.
    pub(super) fn tick_date_authority(&mut self) {
        if !self.date_sync.enabled || !self.ntp_server_mode || self.date_sync.core.rebase_pending()
        {
            return;
        }
        let Some(base) = self.date_sync.core.anchor_ns() else {
            return;
        };
        let now_wall = wall_now_ns();
        // The master's D IN EFFECT (its anchor plus a held slew's displacement).
        let own = self.date_sync.follower.in_effect_ns(base, now_wall);
        let now_ptp = now_wall.wrapping_sub(own);
        let Some(a) = self.date_sync.authority.as_mut() else {
            return;
        };
        let fleet = a.in_effect_ns(now_ptp);
        let announced = a.on_tick(now_ptp);
        self.report_falling_behind(now_ptp);
        let Some(ann) = announced else {
            return;
        };
        let on_line = own == fleet && !self.ptp_offline && !self.in_step_backoff();
        debug!(
            "[DATE] AUTHORITY: micro-correction {:+}us ({}) at PTP {}, seq {}{}",
            ann.date_offset_ns.wrapping_sub(fleet) / 1_000,
            if ann.as_slew().is_some() {
                "slew"
            } else {
                "step"
            },
            ann.effective_ptp_ns,
            ann.seq,
            if on_line { "" } else { " (off the fleet line)" }
        );
        if on_line {
            self.master_schedules_own(ann, base, now_wall);
        }
        // Publish NOW (see `ntp_under_date_authority`): the 31900 server reads this snapshot.
        self.update_shared_status();
    }

    /// #119 follow-up — log the micro-corrections' falling-behind alarm: loudly when it is raised
    /// (and every [`FALLING_BEHIND_WARN_INTERVAL`] while it stays), once when it clears.
    fn report_falling_behind(&mut self, now_ptp: i64) {
        let Some(a) = self.date_sync.authority.as_ref() else {
            return;
        };
        let behind = a.micro().falling_behind();
        let ds = &self.date_sync;
        if behind == ds.falling_behind_logged
            && (!behind
                || ds
                    .falling_behind_warned_at
                    .is_some_and(|t| t.elapsed() < FALLING_BEHIND_WARN_INTERVAL))
        {
            return;
        }
        let cfg = a.micro().config();
        let est = a.micro().estimate(now_ptp);
        if behind {
            warn!(
                "[DATE] AUTHORITY: date correction falling behind: the fleet line is {:+.1} ms off \
                 UTC and drifting {:+.2} ms/min, the micro-corrections hold at most {:.2} ms/min \
                 ({}us per {} s) — no large step is taken below {} ms; check the grandmaster's \
                 frequency and the UTC source",
                est.map(|e| e.error_ns as f64 / 1e6).unwrap_or(0.0),
                est.map(|e| e.trend_ns_per_s * 60.0 / 1e6).unwrap_or(0.0),
                cfg.capacity_ns_per_min() as f64 / 1e6,
                cfg.step_ns / 1_000,
                cfg.interval_ns / 1_000_000_000,
                crate::date_offset::slew_cap_ns(self.date_sync.step_bound_ns) / 1_000_000
            );
            self.date_sync.falling_behind_warned_at = Some(Instant::now());
        } else {
            info!(
                "[DATE] AUTHORITY: date correction caught up: the fleet line is {:+.1} ms off UTC",
                est.map(|e| e.error_ns as f64 / 1e6).unwrap_or(0.0)
            );
            self.date_sync.falling_behind_warned_at = None;
        }
        self.date_sync.falling_behind_logged = behind;
        self.update_shared_status();
    }
}
