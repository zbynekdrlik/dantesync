//! dantesync#119 (1.12) — the [`DateAuthority`]'s DAILY wiring: the correction mode, the nightly
//! step (the [`DailyScheduler`] decides; this turns its step into an announce), the emergency cap
//! and what `/status` reads. Kept beside the authority's core so `date_offset.rs` stays one
//! screenful per concern; the private state it reaches is the authority's own.

use super::*;

impl DateAuthority {
    /// dantesync#119 (1.12) — how the date is corrected (the controller passes the configured
    /// mode; without this the authority runs the 1.11 micro-corrections).
    pub fn with_correction(mut self, mode: CorrectionMode) -> Self {
        self.mode = mode;
        self.daily = match mode {
            CorrectionMode::Daily(cfg) => Some(DailyScheduler::new(cfg)),
            CorrectionMode::Micro => None,
        };
        self
    }

    pub fn correction_mode(&self) -> CorrectionMode {
        self.mode
    }

    /// dantesync#119 (1.12) — the last nightly decision worth a log line (a step, no step
    /// needed, waiting for UTC, a skipped night), once. `None` in micro mode.
    pub fn take_daily_event(&mut self) -> Option<DailyDecision> {
        self.daily_event.take()
    }

    /// dantesync#119 (1.12) — where the next nightly window opens (fleet wall, ns): the open one
    /// while it has not stepped yet. `None` in micro mode.
    pub fn daily_next_window_wall_ns(&self, now_ptp_ns: i64) -> Option<i64> {
        let wall = now_ptp_ns.wrapping_add(self.in_effect_ns(now_ptp_ns));
        self.daily.as_ref().map(|d| d.next_window_wall_ns(wall))
    }

    /// dantesync#119 (1.12) — the last nightly step announced: (the fleet-wall instant it lands
    /// on, its size), ns. `None` before the first, and in micro mode.
    pub fn daily_last_step(&self) -> Option<(i64, i64)> {
        self.daily.as_ref().and_then(|d| d.last_step())
    }

    /// The error beyond which a reading is ABNORMAL and stepped at once: 2 × the step bound
    /// ([`slew_cap_ns`]) in micro mode, the emergency cap in daily mode (dantesync#119, 1.12).
    pub(super) fn abnormal_cap_ns(&self) -> i64 {
        match self.mode {
            CorrectionMode::Daily(cfg) => cfg.emergency_ns,
            CorrectionMode::Micro => slew_cap_ns(self.step_bound_ns),
        }
    }

    /// dantesync#119 (1.12) — an emergency step to `new_offset_ns` was announced to take effect at
    /// `land_ptp_ns`: the nightly scheduler judges its windows afresh from the wall it will leave
    /// ([`DailyScheduler::on_emergency_step`]). Nothing in micro mode.
    pub(super) fn daily_after_emergency(&mut self, land_ptp_ns: i64, new_offset_ns: i64) {
        if let Some(daily) = self.daily.as_mut() {
            daily.on_emergency_step(land_ptp_ns.wrapping_add(new_offset_ns));
        }
    }

    /// dantesync#119 (1.12) — the nightly step: the [`DailyScheduler`] decides on the FLEET wall
    /// (`PTP now + D`; nothing is in flight here, so `D` is `current_ns`) from the micro
    /// estimate at the landing instant — `None` without a UTC reading in the last
    /// [`MICRO_READING_MAX_AGE_NS`]. A step is ONE coordinated step of the whole error, either
    /// direction (never a slew), announced at `land`; the kept readings are compensated at once
    /// (they describe the error once it has landed), as for a micro-correction.
    pub(super) fn daily_tick(&mut self, now_ptp_ns: i64, land: i64) -> Option<DateAnnounce> {
        let wall = now_ptp_ns.wrapping_add(self.current_ns);
        let estimate = if self.micro.settled(now_ptp_ns) {
            self.micro.estimate(land)
        } else {
            None
        };
        let decision = self.daily.as_mut()?.decide(wall, estimate);
        if decision != DailyDecision::Idle {
            self.daily_event = Some(decision);
        }
        let DailyDecision::Step { amount_ns } = decision else {
            return None;
        };
        self.pending = Some((self.current_ns.saturating_add(amount_ns), land));
        let landing_wall = land.wrapping_add(self.current_ns);
        if let Some(daily) = self.daily.as_mut() {
            daily.record_step(landing_wall, amount_ns);
        }
        self.micro.compensate(amount_ns);
        self.micro_kind = false;
        self.seq = self.seq.wrapping_add(1);
        Some(self.announce())
    }
}
