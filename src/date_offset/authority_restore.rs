//! dantesync#126 — the [`DateAuthority`] across a restart of the NTP master, and its coordinated
//! step on request.
//!
//! - [`DateAuthority::persisted`] is what the master saves (its published state, see
//!   [`super::persist`]), [`DateAuthority::restore`] the authority a restarted master builds from
//!   it: the same `D`, the same seq, the change in flight — so followers see the SAME session and
//!   nothing about the fleet date changes.
//! - [`DateAuthority::step_now`] announces the current UTC error as ONE coordinated step, like the
//!   nightly window: the acceptance tests' trigger (a daytime step used to be possible only
//!   through a restart, which was exactly the uncoordinated path).
//!
//! Kept beside the authority's core like `authority_daily.rs`; the private state it reaches is the
//! authority's own.

use super::*;

/// dantesync#126 — why [`DateAuthority::step_now`] announced nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepRefused {
    /// A step or a slew is announced and not complete, or an abnormal correction is being
    /// confirmed: one change of `D` at a time.
    ChangeInFlight,
    /// No settled UTC estimate: fewer than the minimum of fresh readings (the same rule as the
    /// nightly step).
    NoEstimate,
    /// The fleet line is not behind UTC (`error_ns` ≤ 0): a backward correction is slewed or made
    /// at night, never stepped on request (dantesync#119), and there is nothing to step at 0.
    NotBehind { error_ns: i64 },
}

impl DateAuthority {
    /// dantesync#126 — the state to save, as it stands at `now_ptp_ns`: a change due by then has
    /// landed (so the record changes exactly when the published state does).
    pub fn persisted(&self, now_ptp_ns: i64) -> AuthorityState {
        let (d_ns, since_ptp_ns, pending, slew) = self.promoted(now_ptp_ns);
        AuthorityState {
            d_ns,
            since_ptp_ns,
            seq: self.seq,
            pending,
            slew,
            micro: self.micro_kind,
            daily_last_step: self.daily_last_step(),
        }
    }

    /// dantesync#126 — the authority a restarted master builds from its saved state: `D`, its
    /// instant, the seq and the change in flight continue (a change whose instant passed while the
    /// master was down is in effect). Everything the process measured is fresh: the UTC estimate
    /// starts again from the next reading. Configure it like [`new`](Self::new) (the builders),
    /// then [`with_daily_last_step`](Self::with_daily_last_step) after
    /// [`with_correction`](Self::with_correction).
    pub fn restore(
        state: &AuthorityState,
        now_ptp_ns: i64,
        step_bound_ns: i64,
        lead_ns: i64,
    ) -> Self {
        let mut a = DateAuthority::new(state.d_ns, now_ptp_ns, step_bound_ns, lead_ns);
        a.current_since_ptp_ns = state.since_ptp_ns;
        a.seq = state.seq;
        a.pending = state.pending;
        a.slew = state.slew;
        a.micro_kind = state.micro;
        a.promote(now_ptp_ns);
        a
    }

    /// dantesync#126 — the last nightly step of the saved state (daily mode; nothing in micro
    /// mode). Call after [`with_correction`](Self::with_correction), which builds the scheduler.
    pub fn with_daily_last_step(mut self, last: Option<(i64, i64)>) -> Self {
        if let (Some(daily), Some((wall, amount))) = (self.daily.as_mut(), last) {
            daily.restore_last_step(wall, amount);
        }
        self
    }

    /// dantesync#126 — announce the current UTC error NOW as ONE coordinated step,
    /// [`MICRO_LEAD_FACTOR`] leads ahead, like the nightly window: the step every box applies at
    /// that instant. The error is the estimate where the step lands (the same robust line as the
    /// nightly step), and the kept readings are compensated at once. Refused while another change
    /// is in flight, without a settled estimate, and for a fleet not behind UTC.
    pub fn step_now(&mut self, now_ptp_ns: i64) -> Result<DateAnnounce, StepRefused> {
        self.promote(now_ptp_ns);
        if self.pending.is_some() || self.slew.is_some() || self.over_bound.is_some() {
            return Err(StepRefused::ChangeInFlight);
        }
        let land = now_ptp_ns.saturating_add(self.lead_ns.saturating_mul(MICRO_LEAD_FACTOR));
        let estimate = if self.micro.settled(now_ptp_ns) {
            self.micro.estimate(land)
        } else {
            None
        };
        let Some(est) = estimate else {
            return Err(StepRefused::NoEstimate);
        };
        if est.error_ns <= 0 {
            return Err(StepRefused::NotBehind {
                error_ns: est.error_ns,
            });
        }
        self.pending = Some((self.current_ns.saturating_add(est.error_ns), land));
        self.micro.compensate(est.error_ns);
        self.micro_kind = false;
        self.seq = self.seq.wrapping_add(1);
        Ok(self.announce())
    }
}

impl DateFollower {
    /// dantesync#126 — the restarted master aligns its own scheduler with the session it restored
    /// BEFORE it takes the session's announce, when that announce is still ahead (a saved step or
    /// slew whose instant has not come): an unaligned follower ignores a change still ahead, so the
    /// master would never schedule it — nor, staying unaligned, any later one — on its own wall.
    /// `seq` is the seq in effect before that change. No-op once aligned (review round 1).
    pub fn align_with_session(&mut self, seq: u32) {
        if self.adopted_seq.is_none() {
            self.adopted_seq = Some(seq);
        }
    }
}
