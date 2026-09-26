//! dantesync#119 — the controller side of a coordinated date SLEW (`crate::date_offset::DateSlew`).
//!
//! Two pieces make the slew exist on this clock, and together they are its decoupling:
//!
//! - the slew's rate term enters the clock only inside the ONE frequency word: composed with the
//!   servo's word ([`PtpController::compose_slew_word`], from `apply_self_tuning_servo` and the
//!   PTP-offline hold) and re-applied from the 1 ms loop at the slew's start and end instants
//!   ([`PtpController::apply_slew_edge`]), so it switches within one loop iteration on every box;
//! - every PTP sample is de-slewed by the scheduled displacement before either servo reads it
//!   ([`DateSync::deslew_sample`]), so the phase lock and the rate servo see the clock as if no
//!   slew ran and never read the deliberate rate as grandmaster disagreement.
//!
//! When the slew is complete its amount is folded into the anchor ([`DateSync::fold_completed_slew`]),
//! with both servos' measurements kept continuous.

use super::*;

/// The rate servo's measured phase is mod 1 s, so its de-slew accumulator is kept mod 1 s too (a
/// whole second more or less is invisible to it).
const RATE_FOLD_MODULUS_NS: i64 = 1_000_000_000;

/// A slew warning is logged at most this often (the loop runs every 1 ms / 50 µs).
const SLEW_WARN_INTERVAL: Duration = Duration::from_secs(10);

impl DateSync {
    /// dantesync#119 — `D` in effect at `wall_ns` (the anchor plus the held slew's displacement).
    pub(in crate::controller) fn d_in_effect(&self, wall_ns: i64) -> Option<i64> {
        self.core
            .anchor_ns()
            .map(|anchor| self.follower.in_effect_ns(anchor, wall_ns))
    }

    /// dantesync#119 — the held slew's rate term at `wall_ns` (ppm; 0 without a running slew).
    pub(in crate::controller) fn slew_rate_ppm(&self, wall_ns: i64) -> f64 {
        match (self.enabled, self.core.anchor_ns()) {
            (true, Some(anchor)) => self.follower.slew_rate_ppm(anchor, wall_ns),
            _ => 0.0,
        }
    }

    /// dantesync#119 — one PTP sample received at wall `t2_ns`, with the slew's scheduled
    /// displacement removed: `(t2 for the phase lock, t2 for the rate servo)`. The phase lock's is
    /// relative to its anchor; the rate servo's also removes the slews already folded into the
    /// anchor (mod 1 s), so its phase is continuous across a fold. The displacement is taken
    /// at the WALL, so it stays right while a grandmaster change delivers `t1` in another base.
    pub(in crate::controller) fn deslew_sample(&self, t2_ns: i64) -> (i64, i64) {
        if !self.enabled {
            return (t2_ns, t2_ns);
        }
        let Some(anchor) = self.core.anchor_ns() else {
            return (t2_ns, t2_ns);
        };
        let d = self.follower.displacement_at_wall(anchor, t2_ns);
        let raw = t2_ns.wrapping_sub(d);
        (raw, raw.wrapping_sub(self.rate_folded_ns))
    }

    /// dantesync#119 — the held slew is complete: fold it into the anchor (`D` unchanged) and keep
    /// both servos' measurements continuous across it. Returns the folded displacement.
    pub(in crate::controller) fn fold_completed_slew(&mut self, wall_ns: i64) -> Option<i64> {
        let anchor = self.core.anchor_ns()?;
        let fold = self.follower.take_completed_slew(anchor, wall_ns)?;
        self.core.set_anchor(anchor.wrapping_add(fold));
        // Samples already in the windows were de-slewed against the old anchor.
        for s in self.window.iter_mut() {
            *s = s.wrapping_add(fold);
        }
        if let Some(m) = self.pending_median_ns.as_mut() {
            *m = m.wrapping_add(fold);
        }
        self.rate_folded_ns = self
            .rate_folded_ns
            .wrapping_add(fold)
            .rem_euclid(RATE_FOLD_MODULUS_NS);
        Some(fold)
    }

    /// True when a throttled slew warning may be logged now (and records it).
    pub(in crate::controller) fn slew_warn_due(&mut self) -> bool {
        let due = match self.slew_warned_at {
            None => true,
            Some(t) => t.elapsed() >= SLEW_WARN_INTERVAL,
        };
        if due {
            self.slew_warned_at = Some(Instant::now());
        }
        due
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #119 — the ONE frequency word with the held slew's rate term added (clamped to the servo's
    /// envelope), recording which term is in it. `pi_word` is the servo's own word (the phase
    /// lock's, or the rate servo's), i.e. `applied_freq_ppm`.
    pub(in crate::controller) fn compose_slew_word(&mut self, pi_word: f64) -> f64 {
        let now_wall = wall_now_ns();
        let term = self.date_sync.slew_rate_ppm(now_wall);
        let was = self.date_sync.applied_slew_ppm;
        self.date_sync.applied_slew_ppm = term;
        if term == 0.0 {
            return pi_word;
        }
        let total = (pi_word + term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);
        if was == 0.0 {
            // Logged here, whichever path (the loop's edge or a PTP window) switches it on.
            let remaining = self
                .date_sync
                .core
                .anchor_ns()
                .and_then(|a| self.date_sync.follower.slew_remaining_ns(a, now_wall))
                .unwrap_or(0);
            info!(
                "[DATE] slew START: D moves {:+}us at {:+.0} ppm (~{} s), the wall never steps \
                 back — word {:+.3}ppm",
                (if term < 0.0 { -remaining } else { remaining }) / 1_000,
                term,
                (remaining as f64 / (term.abs() * 1_000.0)).round() as i64,
                total
            );
        }
        if total != pi_word + term && self.date_sync.slew_warn_due() {
            warn!(
                "[DATE] the word {:+.3}ppm + the slew {:+.0}ppm leaves the ±{}ppm envelope — \
                 clamped to {:+.3}ppm, the slew runs slower than scheduled here",
                pi_word, term, DRIFT_MAX_PPM, total
            );
        }
        total
    }

    /// #119 — every loop iteration: when the slew's rate term at this instant differs from the one
    /// inside the applied word (the slew started, or ended), re-apply the word with it NOW instead
    /// of at the next PTP window (up to a window late, i.e. up to ~50 µs of relative phase at
    /// 100 ppm). Same composition, same `adjust_frequency` seam: there is no second frequency path.
    pub(in crate::controller) fn apply_slew_edge(&mut self, now_wall: i64) {
        let term = self.date_sync.slew_rate_ppm(now_wall);
        if term == self.date_sync.applied_slew_ppm {
            return;
        }
        let total = self.compose_slew_word(self.applied_freq_ppm);
        if let Err(e) = self.clock.adjust_frequency(1.0 + total / 1_000_000.0) {
            // The next PTP window writes the same composed word again (the servo path); warned
            // at most every few seconds, never once per loop iteration.
            if self.date_sync.slew_warn_due() {
                warn!(
                    "[DATE] applying the slew rate {:+.1}ppm failed: {} — the next PTP window \
                     re-applies it",
                    term, e
                );
            }
            return;
        }
        self.update_shared_status();
    }

    /// #119 — the NTP master while the fleet slews: its own scheduler must hold the authority's
    /// slew. If it missed it (it was off the line at the announce, or an extension was announced
    /// while its step backoff ran), hand it the announce again — accepted only while the master's
    /// `D` is within the absorb tolerance of the fleet line, so this is a continuous catch-up (an
    /// absorb or a scheduled slew), never a step; a larger gap is the ordinary re-alignment after
    /// the slew's end.
    pub(super) fn catch_up_fleet_slew(&mut self) {
        let now_wall = wall_now_ns();
        let (Some(base), Some(a)) = (
            self.date_sync.core.anchor_ns(),
            self.date_sync.authority.as_ref(),
        ) else {
            return;
        };
        let own = self.date_sync.follower.in_effect_ns(base, now_wall);
        let now_ptp = now_wall.wrapping_sub(own);
        let Some(fleet_slew) = a.slew_in_progress(now_ptp) else {
            return;
        };
        if self.date_sync.follower.held_slew().map(|h| h.slew) == Some(fleet_slew) {
            return;
        }
        let gap = a.in_effect_ns(now_ptp).wrapping_sub(own);
        if gap.abs() > crate::date_offset::ABSORB_TOLERANCE_NS {
            return;
        }
        let ann = a.announce();
        match self.date_sync.follower.on_announce(ann, base, now_wall) {
            FollowAction::Absorb { new_anchor_ns } => {
                self.date_sync.core.set_anchor(new_anchor_ns);
            }
            FollowAction::Step { delta_ns, kind } => {
                // Unreachable within the tolerance; handled the ordinary way if it ever is.
                self.apply_date_step(delta_ns, kind, ann.seq);
            }
            _ => {}
        }
        info!(
            "[DATE] the master's own scheduler caught up with the fleet slew (seq {}), {:+}ns off \
             the line — no step",
            ann.seq, gap
        );
    }
}

#[cfg(test)]
mod tests;
