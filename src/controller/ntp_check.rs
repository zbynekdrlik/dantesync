//! dantesync#136 — the periodic NTP check with its burst off the sync loop.
//!
//! `check_ntp_utc_tracking` decides WHEN a check is due, exactly as before; with a burst runner
//! (`main`: the `ntp-burst` thread, [`crate::ntp_burst::NtpBurstThread`]) it only starts the burst
//! there, and the loop goes on. Every iteration [`PtpController::service_ntp_burst`] takes the
//! finished result, when it is in, and applies it with the unchanged rules of the check
//! (`apply_ntp_check`: the same filters, gates, step logic and `/status` fields). Without a runner
//! (a test, a controller built without `main`) the burst is measured inline, as before 1.20.
//!
//! What the inline check never had to consider, because the loop stood still while it measured:
//!
//! - **one burst at a time** — while one is in flight no check starts another; the cadence counts
//!   from its request, as it counted from the inline burst (a burst that outlasts the interval is
//!   followed by the next one at once, as before);
//! - **a wall step while it ran** (a coordinated date step, a boot step) — its samples straddle the
//!   step, so it is not this wall's offset: not used, the next check measures again;
//! - **the boot reading measured again meanwhile** (a deferred boot step, an LI = 3 retry) — the
//!   boot reading comes first (as the inline check never ran while it was pending): not used.
//!
//! Both apply to a MEASUREMENT only: a failed burst has no samples, so it always counts as a
//! failed check (`ntp_consecutive_failures`, `ntp_failed`, `pcap_ntp_active` cleared), as the
//! inline one did.

use super::*;
use crate::ntp_burst::NtpBurstRunner;

/// The periodic burst off the loop: the controller's one `ntp_burst` field.
#[derive(Default)]
pub(in crate::controller) struct NtpBurstState {
    /// `None`: the burst is measured inline (no runner wired).
    runner: Option<Box<dyn NtpBurstRunner>>,
    /// `Some(the wall-step count at its start)` while a burst is in flight.
    in_flight: Option<u64>,
    /// Every successful wall step ([`PtpController::step_wall`]).
    wall_steps: u64,
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// `main`: where the periodic NTP burst is measured (the `ntp-burst` thread).
    pub fn set_ntp_burst_runner(&mut self, runner: Box<dyn NtpBurstRunner>) {
        self.ntp_burst.runner = Some(runner);
    }

    /// A burst is on its way: the check waits for it.
    pub(in crate::controller) fn ntp_burst_in_flight(&self) -> bool {
        self.ntp_burst.in_flight.is_some()
    }

    /// The check is due: start its burst on the runner. True when the runner handled it (started,
    /// or — it could not start one — counted as a failed check, like a burst that failed); false
    /// without a runner (the caller measures inline).
    pub(in crate::controller) fn start_ntp_burst(&mut self) -> bool {
        let Some(runner) = self.ntp_burst.runner.as_mut() else {
            return false;
        };
        if runner.start() {
            self.ntp_burst.in_flight = Some(self.ntp_burst.wall_steps);
        } else {
            self.apply_ntp_check(Err(anyhow::anyhow!(
                "the NTP burst could not start (the ntp-burst thread has ended)"
            )));
        }
        true
    }

    /// Every loop iteration: apply the periodic burst's result once it has arrived.
    pub(in crate::controller) fn service_ntp_burst(&mut self) {
        let Some(started_at_steps) = self.ntp_burst.in_flight else {
            return;
        };
        let Some(result) = self
            .ntp_burst
            .runner
            .as_mut()
            .and_then(|runner| runner.take_result())
        else {
            return;
        };
        self.ntp_burst.in_flight = None;
        // dantesync#136 review: a FAILED burst measured nothing — nothing of it can straddle a
        // step or be superseded by the boot reading. It always counts as a failed check (as the
        // inline one did), or a timed-out burst (~15 s) that met a date micro-step would hold back
        // `ntp_failed` and leave `pcap_ntp_active` set.
        if result.is_err() {
            self.apply_ntp_check(result);
            return;
        }
        if self.boot_ntp_pending() {
            info!(
                "[NTP] periodic burst not used: the boot NTP reading is on its way (it comes first)"
            );
            return;
        }
        if self.ntp_burst.wall_steps != started_at_steps {
            info!(
                "[NTP] periodic burst not used: the wall was stepped while it ran (its samples \
                 straddle the step) — the next check measures again"
            );
            return;
        }
        self.apply_ntp_check(result);
    }

    /// Step the wall clock. Every step goes through here, so a burst that ran across one is known
    /// (only a step that succeeded moved the wall).
    pub(in crate::controller) fn step_wall(&mut self, offset: Duration, sign: i8) -> Result<()> {
        self.clock.step_clock(offset, sign)?;
        self.ntp_burst.wall_steps += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
