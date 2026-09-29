//! dantesync#112 — the controller's PTP liveness: the re-join of the PTP multicast group while no
//! allowed PTP packet comes, and what `/status` says meanwhile.
//!
//! A node used to keep `mode=LOCK is_locked=true` for days with zero PTP frames on the wire: the
//! servo's lock state only changes when a sample window closes, so with no packet it never
//! changed, and the capture was opened once at startup. Now:
//!
//! - "stale" has ONE definition, `ptp_stale_at` (no allowed PTP packet for more than
//!   `PTP_TIMEOUT_SECS`), shared by the offline edge, the clock alarm, the re-join and
//!   `/status`;
//! - while stale, `/status` reports `is_locked=false` and `mode="NTP-only"`, keeps the last
//!   offset, and says how old it is (`last_ptp_rx_age_s`);
//! - the loop re-joins on the pure schedule of `crate::ptp_rejoin` through
//!   `PtpNetwork::rejoin`, which re-opens ONLY the receive path: no clock, servo or date state is
//!   touched, so the fleet date offset survives a loss (a restart would re-derive it).
//!
//! A child module of `controller` so it reaches the controller's private state without growing
//! that file; its state is the one `ptp_liveness` field.

use super::*;
use crate::ptp_rejoin::{rejoin_delay, RejoinSchedule, RejoinStatus, RxWindow};

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
    /// No allowed PTP packet for more than `PTP_TIMEOUT_SECS` at `now`: PTP is stale. Before the
    /// first packet the silence counts from the start.
    pub(super) fn ptp_stale_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_ptp_packet) > Duration::from_secs(PTP_TIMEOUT_SECS)
    }

    /// An allowed PTP packet (one that passed the `gm_allowlist`) arrived at `now`: PTP is live,
    /// the packet is counted, and the next silence is re-joined from scratch.
    pub(super) fn note_allowed_ptp_packet(&mut self, now: Instant) {
        self.last_ptp_packet = now;
        self.ptp_liveness.rx.record(now);
        let attempts = self.ptp_liveness.schedule.reset();
        if attempts > 0 {
            info!(
                "[NET] PTP packets are back after {} re-join attempt(s)",
                attempts
            );
        }
    }

    /// Every loop iteration: re-join the PTP multicast group when PTP is stale and the schedule
    /// says so. An error is logged, published and retried on the schedule; it is never fatal.
    pub(super) fn maybe_rejoin_ptp(&mut self, now: Instant) {
        if !self.ptp_stale_at(now) || !self.ptp_liveness.schedule.due(self.last_ptp_packet, now) {
            return;
        }
        let attempt = self.ptp_liveness.schedule.record_attempt(now);
        warn!(
            "[NET] no PTP announce for {}s -- re-joining PTP multicast (attempt {})",
            now.saturating_duration_since(self.last_ptp_packet)
                .as_secs(),
            attempt
        );
        // Only the receive path is re-opened. The clock, the servos and the date offset are not
        // touched: a re-join must never do what a restart does to the fleet date.
        let result = self.network.rejoin();
        let epoch_s = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match &result {
            Ok(outcome) => {
                info!(
                    "[NET] PTP multicast re-joined on {} ({}){}",
                    outcome.iface,
                    outcome.ip,
                    if outcome.changed {
                        " -- another interface than before"
                    } else {
                        ""
                    }
                );
                self.ptp_liveness.rejoin.record(epoch_s, Ok(outcome));
            }
            Err(e) => {
                let text = format!("{e:#}");
                warn!(
                    "[NET] PTP re-join attempt {} failed: {} -- next attempt in {}s",
                    attempt,
                    text,
                    rejoin_delay(attempt + 1).as_secs()
                );
                self.ptp_liveness.rejoin.record(epoch_s, Err(text.as_str()));
            }
        }
        // Publish the attempt at once (a watchdog sees it without waiting for the 10 s tick).
        if let Ok(mut status) = self.status_shared.write() {
            status.rejoin = self.ptp_liveness.rejoin.clone();
        }
    }

    /// `/status`'s liveness fields at `now`: the packet age, the receive rate, the re-joins.
    pub(super) fn publish_ptp_liveness(&self, status: &mut SyncStatus, now: Instant) {
        status.last_ptp_rx_age_s = self.ptp_liveness.rx.age_s(now);
        status.ptp_rx_pps = self.ptp_liveness.rx.pps(now);
        status.rejoin = self.ptp_liveness.rejoin.clone();
    }
}

#[cfg(test)]
mod tests;
