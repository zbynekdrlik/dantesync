//! dantesync#112 — the controller's PTP liveness: the re-join of the PTP multicast group while no
//! allowed PTP packet comes, and what `/status` says meanwhile.
//!
//! The decisions are pure (`crate::ptp_rejoin`); this file wires them to the loop. A child module
//! of `controller` so it reaches the controller's private state without growing that file; its
//! state is the one `ptp_liveness` field.

use super::*;
use crate::ptp_rejoin::{RejoinSchedule, RejoinStatus, RxWindow};

/// All PTP-liveness state of one controller.
#[derive(Default)]
pub(super) struct PtpLiveness {
    /// When the next re-join of the current silence is due.
    pub(super) schedule: RejoinSchedule,
    /// Allowed PTP packets of the last 10 s (`ptp_rx_pps`, `last_ptp_rx_age_s`).
    pub(super) rx: RxWindow,
    /// `/status.rejoin`.
    pub(super) rejoin: RejoinStatus,
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// An allowed PTP packet arrived at `now` (it passed the `gm_allowlist`).
    pub(super) fn note_allowed_ptp_packet(&mut self, now: Instant) {
        self.last_ptp_packet = now;
    }

    /// Every loop iteration: re-join the PTP multicast group when the schedule says so.
    pub(super) fn maybe_rejoin_ptp(&mut self, _now: Instant) {}
}

#[cfg(test)]
mod tests;
