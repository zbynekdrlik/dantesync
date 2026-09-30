//! dantesync#126 — a follower HOLDS the fleet date through its master's silence.
//!
//! Before 1.15 a follower dropped the authority after 30 s without an applicable reply and went
//! back to its local NTP date path, so a master restart stepped every follower on its own next NTP
//! samples, at scattered instants (dev1 on 30.9.2026: +247.77 ms at 06:02:42Z, 12 s after its
//! loss). Now a follower that has adopted `D` keeps it through `system.date_offset.authority_hold_s`
//! (900 s by default: a restart or reboot of the master plus its PTP re-acquisition): `D`, seq and
//! the last announce stay, a step already scheduled still lands at its instant, and its NTP readings
//! are report-only. The master heard again with the same `D` and seq is re-joined with no step (the
//! ordinary join / absorb rules). Past the hold the 1.14 fallback runs. A follower whose own PTP is
//! offline takes its local NTP path regardless (`ntp_under_date_authority`), as before.

use super::*;

/// What a follower's silence from its authority means now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Silence {
    /// An applicable reply within the loss window: following.
    Heard,
    /// Silent past the loss window, within the hold: keep `D`.
    Hold,
    /// Silent past the loss window and the hold (or never heard): the local NTP date path.
    Lost,
}

/// The decision, pure: `silent_for` = since the last applicable reply (`None`: never).
pub(super) fn authority_silence(
    silent_for: Option<Duration>,
    loss: Duration,
    hold: Duration,
) -> Silence {
    match silent_for {
        None => Silence::Lost,
        Some(s) if s > loss.saturating_add(hold) => Silence::Lost,
        Some(s) if s > loss => Silence::Hold,
        Some(_) => Silence::Heard,
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// dantesync#126 — every loop iteration on a follower: keep the adopted `D` through the
    /// master's silence for the hold, then fall back to the local NTP date path.
    pub(super) fn hold_or_forget_silent_authority(&mut self) {
        if !self.date_sync.follower.adopted() {
            self.date_sync.restart.holding_since = None;
            return;
        }
        let silent_for = self.date_sync.last_applicable_reply.map(|t| t.elapsed());
        let hold = self.date_sync.restart.hold;
        match authority_silence(silent_for, AUTHORITY_LOSS, hold) {
            Silence::Heard => {}
            Silence::Hold => {
                if self.date_sync.restart.holding_since.is_none() {
                    warn!(
                        "[DATE] no applicable date-offset authority reply for {}s — HOLDING the \
                         fleet date offset (seq {}) for up to {}s: no NTP step while the master \
                         restarts or re-acquires PTP",
                        AUTHORITY_LOSS.as_secs(),
                        self.date_sync
                            .follower
                            .adopted_seq()
                            .map(|q| q.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        hold.as_secs()
                    );
                    self.date_sync.restart.holding_since = Some(Instant::now());
                    self.update_shared_status();
                }
            }
            Silence::Lost => {
                warn!(
                    "[DATE] no applicable date-offset authority reply for {}s — back to the local \
                     NTP date path until the master is heard again",
                    AUTHORITY_LOSS.saturating_add(hold).as_secs()
                );
                self.date_sync.follower.forget();
                self.date_sync.last_announce = None;
                self.date_sync.restart.holding_since = None;
                self.update_shared_status();
            }
        }
    }

    /// dantesync#126 — an applicable reply ends a hold (the caller then acts on the announce).
    pub(super) fn end_authority_hold(&mut self) {
        if let Some(since) = self.date_sync.restart.holding_since.take() {
            info!(
                "[DATE] the date-offset authority is heard again after holding for {}s — following \
                 it",
                since.elapsed().as_secs()
            );
        }
    }

    /// dantesync#126 — a follower holding the fleet date offset (its master silent). Not while its
    /// own PTP is offline: it then takes its local NTP path, which a hold never covers.
    pub(in crate::controller) fn holding_date(&self) -> bool {
        self.date_sync.restart.holding_since.is_some() && self.date_sync.follower.adopted()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_authority_is_held_then_lost_and_a_zero_hold_is_the_old_fallback_126() {
        let loss = Duration::from_secs(30);
        let hold = Duration::from_secs(900);
        let s = |secs: u64| Some(Duration::from_secs(secs));
        assert_eq!(authority_silence(s(0), loss, hold), Silence::Heard);
        assert_eq!(authority_silence(s(30), loss, hold), Silence::Heard);
        assert_eq!(authority_silence(s(31), loss, hold), Silence::Hold);
        assert_eq!(authority_silence(s(930), loss, hold), Silence::Hold);
        assert_eq!(authority_silence(s(931), loss, hold), Silence::Lost);
        assert_eq!(authority_silence(None, loss, hold), Silence::Lost);
        // No hold: lost right after the loss window (the 1.14 behaviour).
        assert_eq!(
            authority_silence(s(31), loss, Duration::ZERO),
            Silence::Lost
        );
        assert_eq!(
            authority_silence(s(30), loss, Duration::ZERO),
            Silence::Heard
        );
    }
}
