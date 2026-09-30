//! dantesync#126 — a master restart keeps the fleet date: the NTP master saves its authority's
//! state whenever it changes, and a restarted master restores it at its first PTP lock.
//!
//! The start-up path (`main` → [`PtpController::load_date_state`] → `run_ntp_sync`):
//!
//! - **A readable saved state** skips the boot step (the date is restored, not re-derived from
//!   UTC) and keeps the master off its NTP step path until the first lock: its NTP readings are
//!   report-only meanwhile.
//! - **At the first lock** ([`PtpController::restore_date_authority`], from
//!   `ensure_date_authority`) the state is judged: the same grandmaster, and the first window's
//!   `t2 − t1` within the daily emergency cap of the saved `D`. Accepted: the authority continues
//!   with the same `D`, seq and change in flight, and `realign_master_to_fleet` moves ONLY this
//!   master's wall onto it (a host reboot's RTC wall, the free-run of the restart gap).
//! - **Rejected** (another grandmaster, a grandmaster that restarted its uptime, a wall seconds
//!   off), or **no PTP lock within [`RESTORE_WAIT_FOR_PTP`]**: the boot step runs then, from the
//!   loop ([`PtpController::run_deferred_boot_step`]), and a new session starts at seq 1 — the
//!   path every master took before 1.15, logged loudly.
//! - **No file / an unreadable one**: the boot step runs at start, exactly as before.
//!
//! The accumulated UTC error is left to the next nightly window (or the emergency cap): a seeded
//! daily authority already routes every NTP reading through `on_utc_error`.
//!
//! Saving ([`PtpController::save_date_state_if_changed`]) runs on the master's loop: the record
//! (`DateAuthority::persisted` + the anchor's grandmaster) is written by temp + rename only when it
//! changed; a write error is logged, retried after a backoff, and never fatal.

use super::restart_file;
use super::*;
use crate::date_offset::{AuthorityState, DateOffsetState, RestoreRejected};
use std::path::PathBuf;

/// A saved state still waiting for the first PTP lock after this long is given up: the boot step
/// runs and the master takes its NTP path, as before 1.15 (a master whose PTP never comes must
/// not free-run off UTC for ever). A healthy restart locks in about a minute (strih-lx on
/// 30.9.2026: 65 s).
pub(super) const RESTORE_WAIT_FOR_PTP: Duration = Duration::from_secs(300);

/// After a failed save, the next attempt waits this long (the loop runs every 1 ms / 50 µs).
const SAVE_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// dantesync#126 — the restart state of one controller: the master's saved date offset, and a
/// follower's hold through its master's silence (`hold.rs`) and the step on request
/// (`trigger.rs`).
pub(in crate::controller) struct RestartState {
    /// Where the master saves its date offset (`None`: never saved — a follower, a test).
    pub(in crate::controller) path: Option<PathBuf>,
    /// The saved state read at start, judged at the first PTP lock. While it is here the boot
    /// step and the master's NTP step path are skipped.
    pub(in crate::controller) pending_restore: Option<DateOffsetState>,
    /// When the saved state was read (the wait for the first lock is bounded).
    pub(in crate::controller) loaded_at: Instant,
    /// The authority was restored from the saved state (`/status.date_offset_restored`).
    pub(in crate::controller) restored: bool,
    /// The saved state was rejected (or PTP never came): the boot step runs from the loop, before
    /// the new authority session.
    pub(in crate::controller) boot_step_due: bool,
    /// How far off the saved `D` the first window may read the wall and still restore it (ns).
    pub(in crate::controller) cap_ns: i64,
    /// The record last written (or read at start): written again only when it changes.
    pub(in crate::controller) last_saved: Option<(AuthorityState, [u8; 6])>,
    /// The last failed save (the retry is rate-bounded, the warning logged once per streak).
    pub(in crate::controller) save_failed_at: Option<Instant>,
    /// A follower's hold through a silent master (`system.date_offset.authority_hold_s`).
    pub(in crate::controller) hold: Duration,
    /// When this follower began to HOLD the fleet date offset (its master silent).
    pub(in crate::controller) holding_since: Option<Instant>,
    /// Where the `POST /date/step` requests come from (`main` wires the HTTP route's channel).
    pub(in crate::controller) step_requests:
        Option<std::sync::mpsc::Receiver<crate::date_step_trigger::DateStepRequest>>,
    /// The last step on request: when, and what it did (`/status.date_step_trigger_last`).
    pub(in crate::controller) step_trigger_last: String,
}

impl RestartState {
    pub(in crate::controller) fn new(config: &SystemConfig) -> Self {
        RestartState {
            path: None,
            pending_restore: None,
            loaded_at: Instant::now(),
            restored: false,
            boot_step_due: false,
            cap_ns: config.date_offset.restore_cap_ns(),
            last_saved: None,
            save_failed_at: None,
            hold: config.date_offset.authority_hold(),
            holding_since: None,
            step_requests: None,
            step_trigger_last: String::new(),
        }
    }
}

fn describe_rejection(r: RestoreRejected) -> String {
    match r {
        RestoreRejected::NoGrandmaster => "no grandmaster identity at the first lock".to_string(),
        RestoreRejected::OtherGrandmaster { saved, now } => format!(
            "it belongs to grandmaster {}, this lock is on {}",
            format_mac(&saved),
            format_mac(&now)
        ),
        RestoreRejected::TimeBase { off_ns, cap_ns } => format!(
            "the first PTP window reads the wall {:+.3} ms off the saved D, beyond the {} ms cap \
             (a grandmaster that restarted its uptime, or a wall far off the fleet line)",
            off_ns as f64 / 1e6,
            cap_ns / 1_000_000
        ),
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// dantesync#126 — the NTP master reads the fleet date offset it saved before this restart
    /// (`main`, before `run_ntp_sync`), and saves it at `path` from now on. A readable state
    /// skips the boot step and is restored at the first PTP lock; without one nothing changes.
    pub fn load_date_state(&mut self, path: PathBuf) {
        // RED stub (#126): the saved state is not read yet.
        self.date_sync.restart.path = Some(path);
    }

    /// dantesync#126 — a saved state waits for the first PTP lock: the boot step is skipped.
    pub(in crate::controller) fn boot_step_deferred(&self) -> bool {
        false // RED stub (#126)
    }

    /// dantesync#126 — every loop iteration on the master, before anything needs an anchor: give
    /// up a saved state that has waited too long for PTP, and run a deferred boot step.
    pub(super) fn service_date_restart(&mut self) {
        // RED stub (#126)
    }

    /// dantesync#126 — the boot step a rejected (or abandoned) saved state deferred: the start-up
    /// `run_ntp_sync` path, taken now from the loop. `D` moves with the wall (the phase lock sees
    /// no disturbance), and the new authority is built on the stepped anchor.
    pub(super) fn run_deferred_boot_step(&mut self) {
        // RED stub (#126)
    }

    /// dantesync#126 — the first anchor of a master with a saved state: judge it, and on success
    /// make the restored authority this master's (true). A rejection defers the boot step (false:
    /// no authority until it has run).
    pub(super) fn restore_date_authority(&mut self, anchor: i64, now_wall: i64) -> bool {
        let _ = (anchor, now_wall);
        false // RED stub (#126): never restored
    }

    /// dantesync#126 — the master saves its authority's state when it changed (every loop
    /// iteration; the write only on a change): `D` in effect, seq, the change in flight, the last
    /// nightly step, and the grandmaster of the anchor. Nothing mid re-anchor.
    pub(super) fn save_date_state_if_changed(&mut self) {
        // RED stub (#126): nothing is saved
    }
}
