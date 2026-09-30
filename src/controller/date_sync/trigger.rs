//! dantesync#126 — the coordinated date step ON REQUEST, the controller side: the HTTP route
//! (`crate::date_step_trigger`, `POST /date/step`, loopback only) hands each request to the sync
//! loop over a channel; the master's authority announces the current UTC error as ONE coordinated
//! step two leads ahead (`DateAuthority::step_now`), the master schedules it on its own wall like
//! every announce and publishes it at once, and every follower applies it at the instant through
//! the ordinary path. Anything else is refused with the reason (never a partial action).

use super::*;
use crate::date_offset::StepRefused;
use crate::date_step_trigger::{DateStepOutcome, DateStepRequest};
use std::sync::mpsc;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// dantesync#126 — where `POST /date/step` requests come from (`main` wires the HTTP route's
    /// channel; every node answers them, only the NTP master can accept one).
    pub fn set_date_step_requests(&mut self, requests: mpsc::Receiver<DateStepRequest>) {
        self.date_sync.restart.step_requests = Some(requests);
    }

    /// dantesync#126 — every loop iteration: answer each queued request (the HTTP thread waits).
    pub(in crate::controller) fn serve_date_step_requests(&mut self) {
        let Some(rx) = self.date_sync.restart.step_requests.as_ref() else {
            return;
        };
        let requests: Vec<DateStepRequest> = rx.try_iter().collect();
        for req in requests {
            // Claimed once: a request its HTTP side abandoned (it answered 503, "nothing was
            // announced") is never acted on (review rounds 1-2).
            let outcome = if req.take() {
                self.date_step_on_request()
            } else {
                self.record_date_step(DateStepOutcome::Refused {
                    reason: "the request expired before the sync loop took it: nothing announced"
                        .to_string(),
                })
            };
            let _ = req.reply.send(outcome);
        }
    }

    /// dantesync#126 — one request: the coordinated step, or why not.
    pub(in crate::controller) fn date_step_on_request(&mut self) -> DateStepOutcome {
        let outcome = self.try_date_step_on_request();
        self.record_date_step(outcome)
    }

    /// dantesync#126 — record a request's outcome (`/status.date_step_trigger_last`, the log) and
    /// publish.
    fn record_date_step(&mut self, outcome: DateStepOutcome) -> DateStepOutcome {
        let now = crate::date_offset::format_utc_rfc3339(wall_now_ns());
        self.date_sync.restart.step_trigger_last = match &outcome {
            DateStepOutcome::Accepted {
                amount_ns,
                due_in_ms,
                seq,
                ..
            } => format!(
                "{now} accepted: a coordinated step of {:+.3} ms, seq {seq}, lands in {} ms",
                *amount_ns as f64 / 1e6,
                due_in_ms
            ),
            DateStepOutcome::Refused { reason } => format!("{now} refused: {reason}"),
        };
        if let DateStepOutcome::Refused { reason } = &outcome {
            info!("[DATE] date step on request refused: {}", reason);
        }
        self.update_shared_status();
        outcome
    }

    fn try_date_step_on_request(&mut self) -> DateStepOutcome {
        let refuse = |reason: &str| DateStepOutcome::Refused {
            reason: reason.to_string(),
        };
        if !self.date_sync.enabled {
            return refuse("the legacy clock discipline has no fleet date offset");
        }
        if !self.ntp_server_mode {
            return refuse(
                "this node is not the fleet date-offset authority (ask the NTP master, locally)",
            );
        }
        if self.date_sync.authority.is_none() {
            return refuse(
                "the date authority is not up yet: the master restores or starts it at its first \
                 PTP lock",
            );
        }
        if self.ptp_offline
            || !self.date_sync.core.engaged()
            || self.date_sync.core.rebase_pending()
        {
            return refuse("the master has no PTP phase lock right now");
        }
        let Some(base) = self.date_sync.core.anchor_ns() else {
            return refuse("the master has no PTP phase lock right now");
        };
        let now_wall = wall_now_ns();
        // The master's D IN EFFECT (its anchor plus a held slew's displacement).
        let own = self.date_sync.follower.in_effect_ns(base, now_wall);
        let now_ptp = now_wall.wrapping_sub(own);
        let backoff = self.in_step_backoff();
        let Some(a) = self.date_sync.authority.as_mut() else {
            return refuse("this node is not the fleet date-offset authority");
        };
        let fleet = a.in_effect_ns(now_ptp);
        if own != fleet || backoff {
            return refuse("the master is off the fleet line (it re-aligns its own wall first)");
        }
        match a.step_now(now_ptp) {
            Ok(ann) => {
                let amount = ann.date_offset_ns.wrapping_sub(fleet);
                let due_ms = ann.effective_ptp_ns.wrapping_sub(now_ptp) / 1_000_000;
                warn!(
                    "[DATE] AUTHORITY: date step ON REQUEST {:+.3} ms (the whole UTC error, a \
                     coordinated step) at PTP {} (in {} s), seq {}",
                    amount as f64 / 1e6,
                    ann.effective_ptp_ns,
                    due_ms / 1_000,
                    ann.seq
                );
                self.master_schedules_own(ann, base, now_wall);
                DateStepOutcome::Accepted {
                    amount_ns: amount,
                    land_ptp_ns: ann.effective_ptp_ns,
                    due_in_ms: due_ms,
                    seq: ann.seq,
                }
            }
            Err(StepRefused::ChangeInFlight) => {
                refuse("a date change is already in flight (one at a time)")
            }
            Err(StepRefused::NoEstimate) => refuse(
                "no settled UTC estimate yet (at least six fresh NTP readings in five minutes)",
            ),
            Err(StepRefused::NotBehind { error_ns }) => DateStepOutcome::Refused {
                reason: format!(
                    "the fleet is not behind UTC ({:+.3} ms): a backward correction is never \
                     stepped on request (it is made at night)",
                    error_ns as f64 / 1e6
                ),
            },
        }
    }
}
