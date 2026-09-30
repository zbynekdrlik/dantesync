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
//! The accumulated UTC error is left to the authority's coordinated correction — the next nightly
//! window or the emergency cap in daily mode (the default), the micro-corrections in micro mode —
//! because a restored authority routes every NTP reading through `on_utc_error` like any other.
//!
//! Saving ([`PtpController::save_date_state_if_changed`]) runs on the master's loop: the record
//! (`DateAuthority::persisted` + the anchor's grandmaster) is written by temp + rename when it
//! changed and at least every 10 minutes (a record over a day old is not restored); a write error
//! is logged, retried after a backoff, and never fatal. A node that starts as a non-master removes
//! a leftover record ([`PtpController::remove_stale_date_state`]).

use super::restart_file;
use super::*;
use crate::date_offset::{AuthorityState, DateOffsetState, RestoreRejected};
use std::path::{Path, PathBuf};

/// A saved state still waiting for the first PTP lock after this long is given up: the boot step
/// runs and the master takes its NTP path, as before 1.15 (a master whose PTP never comes must
/// not free-run off UTC for ever). A healthy restart locks in about a minute (strih-lx on
/// 30.9.2026: 65 s).
pub(super) const RESTORE_WAIT_FOR_PTP: Duration = Duration::from_secs(300);

/// After a failed save, the next attempt waits this long (the loop runs every 1 ms / 50 µs).
const SAVE_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// The saved state is rewritten at least this often, unchanged or not, so its `written_wall_ns`
/// says the master was running then (the restore refuses a record over a day old — review round
/// 1) and a deleted file comes back.
const SAVE_HEARTBEAT: Duration = Duration::from_secs(600);

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
    /// The record last written (or read at start): written again when it changes, or after
    /// [`SAVE_HEARTBEAT`] since `last_saved_at`.
    pub(in crate::controller) last_saved: Option<(AuthorityState, [u8; 6])>,
    pub(in crate::controller) last_saved_at: Option<Instant>,
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
            last_saved_at: None,
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
        RestoreRejected::Stale { age_ns } => format!(
            "it is {} h old (the fleet may have had another master since)",
            age_ns / 3_600_000_000_000
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
        let rs = &mut self.date_sync.restart;
        rs.loaded_at = Instant::now();
        match restart_file::read(&path) {
            Ok(Some(state)) => {
                let age_s = wall_now_ns()
                    .saturating_sub(state.written_wall_ns)
                    .max(0)
                    / 1_000_000_000;
                info!(
                    "[DATE] saved fleet date offset found at {}: D={}ns seq {} (grandmaster {}, \
                     saved {} s ago) — the boot step is skipped; it is restored at the first PTP \
                     lock, so this restart does not move the fleet date",
                    path.display(),
                    state.authority.d_ns,
                    state.authority.seq,
                    format_mac(&state.gm_uuid),
                    age_s
                );
                rs.last_saved = Some((state.authority, state.gm_uuid));
                rs.pending_restore = Some(state);
            }
            Ok(None) => info!(
                "[DATE] no saved fleet date offset at {} (the first start of 1.15, or this node was \
                 not the master): the boot step and a new authority session run as before",
                path.display()
            ),
            Err(e) => warn!(
                "[DATE] the saved fleet date offset at {} is not usable ({}): not restored — the \
                 boot step and a new authority session run as before",
                path.display(),
                e
            ),
        }
        rs.path = Some(path);
    }

    /// dantesync#126 — a saved state waits for the first PTP lock: the boot step is skipped.
    pub(in crate::controller) fn boot_step_deferred(&self) -> bool {
        self.date_sync.restart.pending_restore.is_some()
    }

    /// dantesync#126 — is the boot step of `offset_us` skipped for the saved state? A wall more
    /// than twice the restore cap off UTC cannot be on the fleet line (the authority keeps the line
    /// within the cap of UTC), so that state could never be restored: it is dropped at once and the
    /// boot step runs at start, instead of the master serving a wall seconds off UTC until its first
    /// lock rejects it (review round 1).
    pub(in crate::controller) fn skip_boot_step_for(&mut self, offset_us: i64) -> bool {
        if !self.boot_step_deferred() {
            return false;
        }
        let limit_ns = self.date_sync.restart.cap_ns.saturating_mul(2);
        if offset_us.unsigned_abs().saturating_mul(1_000) > limit_ns.unsigned_abs() {
            warn!(
                "[DATE] the boot NTP offset {:+}us is more than twice the {} ms restore cap: this \
                 wall cannot be on the fleet line — the saved fleet date offset is NOT restored, \
                 the boot step runs now (the pre-1.15 path)",
                offset_us,
                self.date_sync.restart.cap_ns / 1_000_000
            );
            self.date_sync.restart.pending_restore = None;
            return false;
        }
        true
    }

    /// dantesync#126 — this node is not the fleet's master after all (`main`: its NTP server did
    /// not start): the saved state it read is not restored, and the boot step it skipped runs now.
    /// Without this it would never step to UTC (review round 1).
    pub fn abandon_date_state(&mut self) {
        if self.date_sync.restart.pending_restore.take().is_some() {
            warn!(
                "[DATE] this node is not the NTP master after all: the saved fleet date offset is \
                 not restored — the boot step runs now"
            );
            self.date_sync.restart.boot_step_due = true;
            self.run_deferred_boot_step();
        }
    }

    /// dantesync#126 — a node that starts as a NON-master removes a saved date offset left from an
    /// earlier stint as the master: by the time it is the master again, the fleet's session is
    /// another one (review round 1). A missing file is the normal case.
    pub fn remove_stale_date_state(&mut self, path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => warn!(
                "[DATE] removed {}: a saved fleet date offset from an earlier stint as the NTP \
                 master (this node is not the master now)",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(
                "[DATE] could not remove the stale saved fleet date offset {}: {}",
                path.display(),
                e
            ),
        }
    }

    /// dantesync#126 — every loop iteration, before anything needs an anchor: give up a saved state
    /// that has waited too long for PTP, and run a deferred boot step.
    pub(super) fn service_date_restart(&mut self) {
        let rs = &mut self.date_sync.restart;
        if rs.pending_restore.is_some() && rs.loaded_at.elapsed() >= RESTORE_WAIT_FOR_PTP {
            warn!(
                "[DATE] no PTP lock {} s after start: the saved fleet date offset is NOT restored \
                 — the boot step runs now and a new authority session starts at seq 1 (the \
                 pre-1.15 path)",
                RESTORE_WAIT_FOR_PTP.as_secs()
            );
            rs.pending_restore = None;
            rs.boot_step_due = true;
        }
        self.run_deferred_boot_step();
    }

    /// dantesync#126 — the boot step a rejected (or abandoned) saved state deferred: the start-up
    /// `run_ntp_sync` path, taken now from the loop. `D` moves with the wall (the phase lock sees
    /// no disturbance), and the new authority is built on the stepped anchor.
    pub(super) fn run_deferred_boot_step(&mut self) {
        if !self.date_sync.restart.boot_step_due {
            return;
        }
        self.date_sync.restart.boot_step_due = false;
        let measurement = match self.ntp.get_offset() {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    "[DATE] deferred boot step: NTP failed ({}) — the new authority starts on the \
                     current wall",
                    e
                );
                return;
            }
        };
        let offset_us = if measurement.sign > 0 {
            measurement.offset.as_micros() as i64
        } else {
            -(measurement.offset.as_micros() as i64)
        };
        self.record_ntp_success(offset_us, &measurement);
        warn!(
            "[DATE] the boot step runs now (NTP offset {:+}us): the saved fleet date offset was \
             not restored",
            offset_us
        );
        if self.step_boot_offset(measurement.offset, measurement.sign) {
            // D moves with the wall, exactly: the phase lock sees no disturbance.
            let sign = if measurement.sign > 0 { 1 } else { -1 };
            let delta_ns = (measurement.offset.as_nanos() as i64).saturating_mul(sign);
            self.date_sync.core.note_step(delta_ns);
            self.reset_ptp_measurement_after_step();
            self.ntp_offset_samples.clear();
            self.ntp_pending_step = None;
        }
    }

    /// dantesync#126 — the first anchor of a master with a saved state: judge it, and on success
    /// make the restored authority this master's (true). A rejection defers the boot step (false:
    /// no authority until it has run).
    pub(super) fn restore_date_authority(&mut self, anchor: i64, now_wall: i64) -> bool {
        let Some(saved) = self.date_sync.restart.pending_restore.take() else {
            return false;
        };
        let checked = saved.validate_restore(
            self.date_sync.anchor_gm,
            anchor,
            now_wall,
            self.date_sync.restart.cap_ns,
        );
        let off = match checked {
            Ok(off) => off,
            Err(reason) => {
                warn!(
                    "[DATE] the saved fleet date offset is NOT restored: {} — the boot step runs \
                     now and a new authority session starts at seq 1 (every follower re-joins it: \
                     the pre-1.15 path)",
                    describe_rejection(reason)
                );
                self.date_sync.restart.boot_step_due = true;
                return false;
            }
        };
        let now_ptp = now_wall.wrapping_sub(anchor);
        let authority = DateAuthority::restore(
            &saved.authority,
            now_ptp,
            self.date_sync.step_bound_ns,
            self.date_sync.step_lead_ns,
        )
        .with_slew_ppm(self.date_sync.slew_ppm)
        .with_micro(self.date_sync.micro)
        .with_correction(self.date_sync.correction)
        .with_daily_last_step(saved.authority.daily_last_step);
        let base = self.date_sync.core.anchor_ns().unwrap_or(anchor);
        // A saved step or slew still ahead is scheduled on this master's own wall only once its
        // scheduler is aligned with the session (review round 1): else it — and every later
        // change — would reach this master as a late re-join.
        let ahead = authority.pending_step_ns(now_ptp).is_some()
            || authority
                .slew_in_progress(now_ptp)
                .is_some_and(|sl| sl.start_ptp_ns > now_ptp);
        if ahead {
            self.date_sync
                .follower
                .align_with_session(authority.seq().wrapping_sub(1));
        }
        let act = self
            .date_sync
            .follower
            .on_announce(authority.announce(), base, now_wall);
        debug!(
            "[DATE] master aligned with its restored authority: {:?}",
            act
        );
        warn!(
            "[DATE] RESTORED the fleet date offset saved {} s ago: D={}ns seq {} (grandmaster {}) \
             — the fleet date continues through this restart; this master's wall is {:+}us off it \
             and re-joins it alone; the UTC error is left to the authority's coordinated \
             correction (the nightly window in daily mode)",
            now_wall.saturating_sub(saved.written_wall_ns).max(0) / 1_000_000_000,
            authority.in_effect_ns(now_ptp),
            authority.seq(),
            format_mac(&saved.gm_uuid),
            off / 1_000
        );
        self.date_sync.authority = Some(authority);
        self.date_sync.restart.restored = true;
        // Followers hold the fleet D meanwhile: publish it at once.
        self.update_shared_status();
        true
    }

    /// dantesync#126 — the master saves its authority's state when it changed (every loop
    /// iteration; the write only on a change): `D` in effect, seq, the change in flight, the last
    /// nightly step, and the grandmaster of the anchor. Nothing mid re-anchor.
    pub(super) fn save_date_state_if_changed(&mut self) {
        let ds = &self.date_sync;
        let (Some(path), Some(a), Some(gm)) = (
            ds.restart.path.as_ref(),
            ds.authority.as_ref(),
            ds.anchor_gm,
        ) else {
            return;
        };
        if ds.core.rebase_pending() {
            return;
        }
        let now_wall = wall_now_ns();
        let Some(own) = ds.d_in_effect(now_wall) else {
            return;
        };
        let now_ptp = now_wall.wrapping_sub(own);
        let state = a.persisted(now_ptp);
        let recent = ds
            .restart
            .last_saved_at
            .is_some_and(|t| t.elapsed() < SAVE_HEARTBEAT);
        if ds.restart.last_saved == Some((state, gm)) && recent {
            return;
        }
        if ds
            .restart
            .save_failed_at
            .is_some_and(|t| t.elapsed() < SAVE_RETRY_INTERVAL)
        {
            return;
        }
        let record = DateOffsetState {
            authority: state,
            gm_uuid: gm,
            written_wall_ns: now_wall,
            written_ptp_ns: now_ptp,
        };
        match restart_file::write_atomic(path, &record) {
            Ok(()) => {
                debug!(
                    "[DATE] saved the fleet date offset: D={}ns seq {}",
                    state.d_ns, state.seq
                );
                if self.date_sync.restart.save_failed_at.take().is_some() {
                    info!("[DATE] the fleet date offset is saved again");
                }
                self.date_sync.restart.last_saved = Some((state, gm));
                self.date_sync.restart.last_saved_at = Some(Instant::now());
            }
            Err(e) => {
                if self.date_sync.restart.save_failed_at.is_none() {
                    warn!(
                        "[DATE] could not save the fleet date offset to {}: {} — a restart of this \
                         master would step the fleet (retrying every {} s)",
                        path.display(),
                        e,
                        SAVE_RETRY_INTERVAL.as_secs()
                    );
                }
                self.date_sync.restart.save_failed_at = Some(Instant::now());
            }
        }
    }
}
