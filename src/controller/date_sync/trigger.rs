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
            let outcome = self.date_step_on_request();
            // The HTTP thread may have given up (timeout); nothing to do then.
            let _ = req.reply.send(outcome);
        }
    }

    /// dantesync#126 — one request: the coordinated step, or why not.
    pub(in crate::controller) fn date_step_on_request(&mut self) -> DateStepOutcome {
        let outcome = self.try_date_step_on_request();
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
        DateStepOutcome::Refused {
            reason: "not implemented yet (RED stub, #126)".to_string(),
        }
    }
}
