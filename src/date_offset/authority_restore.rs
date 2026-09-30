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
        let _ = now_ptp_ns;
        // RED stub (#126): nothing is saved yet.
        AuthorityState {
            d_ns: 0,
            since_ptp_ns: 0,
            seq: 0,
            pending: None,
            slew: None,
            micro: false,
            daily_last_step: None,
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
        // RED stub (#126): a fresh authority, as before 1.15.
        DateAuthority::new(state.d_ns, now_ptp_ns, step_bound_ns, lead_ns)
    }

    /// dantesync#126 — the last nightly step of the saved state (daily mode; nothing in micro
    /// mode). Call after [`with_correction`](Self::with_correction), which builds the scheduler.
    pub fn with_daily_last_step(self, last: Option<(i64, i64)>) -> Self {
        let _ = last;
        self // RED stub (#126)
    }

    /// dantesync#126 — announce the current UTC error NOW as ONE coordinated step,
    /// [`MICRO_LEAD_FACTOR`] leads ahead, like the nightly window: the step every box applies at
    /// that instant. The error is the estimate where the step lands (the same robust line as the
    /// nightly step), and the kept readings are compensated at once. Refused while another change
    /// is in flight, without a settled estimate, and for a fleet not behind UTC.
    pub fn step_now(&mut self, now_ptp_ns: i64) -> Result<DateAnnounce, StepRefused> {
        let _ = now_ptp_ns;
        Err(StepRefused::NoEstimate) // RED stub (#126)
    }
}
