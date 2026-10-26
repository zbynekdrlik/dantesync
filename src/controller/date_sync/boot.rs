//! dantesync#134 — fast boot sync: the PTP loop never waits for the boot NTP reading.
//!
//! Before 1.19 `main` measured the boot NTP offset (`run_ntp_sync`, a blocking burst) BEFORE the
//! PTP loop started: 12 s on strih-pp to an internet upstream at boot (25.10.2026), with nothing
//! PTP running meanwhile. Now `main` measures it on its own thread and hands it over on a channel
//! ([`PtpController::start_boot_ntp`]); the loop applies it once it arrives
//! ([`PtpController::service_boot_ntp`]) with the same rules as before:
//!
//! - a master with a readable saved fleet date offset skips the step (it is restored at its first
//!   PTP window instead), unless the reading is beyond twice the restore cap;
//! - a node already ON the fleet date line when the reading arrives (a restored or new master
//!   authority, a follower that adopted the master's D) only records it: a step now would move it
//!   off the line it just joined;
//! - otherwise the boot step, and `D` moves with the wall if the phase lock anchored already (the
//!   loop sees no disturbance).
//!
//! While the reading is pending (at most [`BOOT_NTP_WAIT`]) the loop's periodic NTP check does not
//! run (it would block the loop on the same unanswered upstream), a master without a saved state
//! does not start its authority session (the boot step places the wall it starts on), and a
//! deferred boot step waits for this reading instead of measuring its own.

use super::*;
use crate::ntp::NtpMeasurement;
use std::sync::mpsc::{Receiver, TryRecvError};

/// One boot NTP reading, measured off the loop (`main`'s boot-NTP thread).
pub type BootNtpResult = std::result::Result<NtpMeasurement, BootNtpError>;

/// Why a boot NTP reading failed.
#[derive(Clone, Debug, PartialEq)]
pub struct BootNtpError {
    pub message: String,
    /// The server answered "clock unsynchronized" (LI = 3): a master whose own wall is not placed
    /// yet (dantesync#134 re-review: a follower asks again a few seconds later).
    pub server_unsynchronized: bool,
}

impl BootNtpError {
    /// Any other failure (a timeout, a dead thread, no answer).
    pub fn other(message: impl Into<String>) -> Self {
        BootNtpError {
            message: message.into(),
            server_unsynchronized: false,
        }
    }

    /// The NTP client's error, classified (`crate::ntp::is_server_unsynchronized`).
    pub fn from_ntp(e: &anyhow::Error) -> Self {
        BootNtpError {
            message: e.to_string(),
            server_unsynchronized: crate::ntp::is_server_unsynchronized(e),
        }
    }
}

impl std::fmt::Display for BootNtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The loop waits this long for the boot reading; after it the node goes on as if the boot NTP
/// had failed (the pre-1.19 "NTP Sync failed" path: the authority starts on the current wall, the
/// periodic NTP takes over).
pub(in crate::controller) const BOOT_NTP_WAIT: Duration = Duration::from_secs(60);

/// A boot reading kept for a deferred boot step is used up to this old (the wall was not stepped
/// since: the saved state skipped it); older, the step is measured again on a thread.
/// dantesync#134 re-review: a follower whose master answers "unsynchronized" asks again this
/// often, at most [`BOOT_UNSYNC_RETRIES`] times (~1 min).
pub(in crate::controller) const BOOT_UNSYNC_RETRY: Duration = Duration::from_secs(5);
pub(in crate::controller) const BOOT_UNSYNC_RETRIES: usize = 12;

pub(in crate::controller) const BOOT_READING_REUSE: Duration = Duration::from_secs(120);

/// The boot reading still on its way.
pub(in crate::controller) struct BootNtpWait {
    rx: Receiver<BootNtpResult>,
    pub(in crate::controller) since: Instant,
}

/// `main`'s way to measure a boot NTP reading again on a thread (dantesync#134 review: a deferred
/// boot step must never measure inside the loop).
pub type BootNtpSpawner = Box<dyn FnMut() -> Receiver<BootNtpResult>>;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// `main`: the boot NTP reading will arrive on `rx` (the PTP loop starts at once).
    pub fn start_boot_ntp(&mut self, rx: Receiver<BootNtpResult>) {
        self.date_sync.restart.boot_ntp = Some(BootNtpWait {
            rx,
            since: Instant::now(),
        });
    }

    /// `main`: how to measure the boot reading again on a thread.
    pub fn set_boot_ntp_spawner(&mut self, spawner: BootNtpSpawner) {
        self.date_sync.restart.boot_ntp_spawner = Some(spawner);
    }

    /// The boot reading has not arrived yet (and the wait for it is not over).
    pub(in crate::controller) fn boot_ntp_pending(&self) -> bool {
        self.date_sync.restart.boot_ntp.is_some()
    }

    /// Every loop iteration: apply the boot NTP reading once it has arrived; give up waiting after
    /// [`BOOT_NTP_WAIT`].
    pub fn service_boot_ntp(&mut self) {
        let Some(wait) = self.date_sync.restart.boot_ntp.as_ref() else {
            return;
        };
        let result = match wait.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) if wait.since.elapsed() < BOOT_NTP_WAIT => return,
            Err(TryRecvError::Empty) => Err(BootNtpError::other(format!(
                "no boot NTP reading within {} s",
                BOOT_NTP_WAIT.as_secs()
            ))),
            Err(TryRecvError::Disconnected) => {
                Err(BootNtpError::other("the boot NTP thread ended"))
            }
        };
        let waited = wait.since.elapsed();
        self.date_sync.restart.boot_ntp = None;
        // Review of PR 135: the reading is applied in this iteration, so the loop's periodic NTP
        // check (allowed again from here) never repeats the burst at once.
        info!(
            "[NTP] boot reading after {:.1} s (the PTP loop ran meanwhile)",
            waited.as_secs_f64()
        );
        self.apply_boot_ntp(result);
    }

    /// The boot NTP reading: the boot step by the start-up rules (the module doc). `run_ntp_sync`
    /// (the synchronous form), [`Self::service_boot_ntp`] and a deferred boot step all end here.
    pub(in crate::controller) fn apply_boot_ntp(&mut self, result: BootNtpResult) {
        let measurement = match result {
            Ok(m) => m,
            Err(e) => {
                // Review of PR 135: a failed / timed-out / dead reading is the pre-1.19 "NTP Sync
                // failed" — the node starts on its current wall (a deferred boot step included),
                // and the loop never measures again inline for it.
                warn!(
                    "NTP Sync failed: {} — no boot step: this node starts on its current wall (the \
                     periodic NTP takes over)",
                    e
                );
                self.date_sync.restart.boot_step_due = false;
                self.update_shared_status();
                return;
            }
        };
        let offset = measurement.offset;
        let sign = measurement.sign;
        let sign_str = if sign > 0 { "+" } else { "-" };
        info!(
            "NTP Sync: Offset {}{:?} (spread:{}us samples:{})",
            sign_str, offset, measurement.spread_us, measurement.sample_count
        );
        // A deferred boot step (a rejected saved state) is THIS step: it does not measure again.
        self.date_sync.restart.boot_step_due = false;
        if self.on_fleet_date_line() {
            // Review of PR 135: measured before the wall was placed (a step since), so it is not
            // this wall's offset — logged only: published, it would show the placement as an NTP
            // error and serve a reference epoch that far off.
            info!(
                "[DATE] boot reading {}{:?} NOT used: this node is on the fleet date line already \
                 (placed at its first PTP windows) — logged only, never published",
                sign_str, offset
            );
            self.update_shared_status();
            return;
        }
        // #68: the boot measurement is a real measurement — publish it.
        let offset_us = if sign > 0 {
            offset.as_micros() as i64
        } else {
            -(offset.as_micros() as i64)
        };
        self.record_ntp_success(offset_us, &measurement);
        if offset.as_millis() > BOOT_STEP_THRESHOLD_MS && self.skip_boot_step_for(offset_us) {
            // dantesync#126: the NTP master restores its saved fleet date offset at the first PTP
            // windows instead — a restart must not move the fleet date. The reading is kept: a
            // rejected restore takes its boot step from it (review of PR 135).
            info!(
                "[DATE] boot step of {}{:?} SKIPPED: the saved fleet date offset is restored at the \
                 first PTP windows (the date authority corrects the UTC error, coordinated, as if \
                 the master had not restarted)",
                sign_str, offset
            );
            self.date_sync.restart.boot_reading = Some((measurement, Instant::now()));
            self.update_shared_status();
            return;
        }
        if self.step_boot_offset(offset, sign) {
            // Review of PR 135: the samples taken before the step describe the old wall — dropped
            // whether or not the phase lock anchored yet (a window mixing both anchors D off the
            // wall; on the legacy servo the step reads as a rate). D moves with the wall when
            // anchored, exactly — the loop sees no disturbance.
            if self.date_sync.core.anchor_ns().is_some() {
                let delta_ns =
                    (offset.as_nanos() as i64).saturating_mul(if sign > 0 { 1 } else { -1 });
                self.date_sync.core.note_step(delta_ns);
            }
            self.reset_ptp_measurement_after_step();
            self.ntp_offset_samples.clear();
            self.ntp_pending_step = None;
        }
        self.update_shared_status();
    }

    /// Review of PR 135 — a deferred boot step (a rejected or given-up saved state): from the
    /// boot reading taken before (if it is younger than [`BOOT_READING_REUSE`]), else measured
    /// again on a thread (`main`'s spawner). True when handled here; false = no spawner (a test,
    /// `run_ntp_sync`'s callers): the caller measures inline as before.
    pub(in crate::controller) fn deferred_boot_step_off_the_loop(&mut self) -> bool {
        if let Some((m, at)) = self.date_sync.restart.boot_reading.take() {
            if at.elapsed() < BOOT_READING_REUSE {
                info!(
                    "[DATE] the deferred boot step uses the boot reading taken {:.0} s ago",
                    at.elapsed().as_secs_f64()
                );
                self.apply_boot_ntp(Ok(m));
                return true;
            }
        }
        let Some(spawn) = self.date_sync.restart.boot_ntp_spawner.as_mut() else {
            return false;
        };
        info!("[DATE] the deferred boot step measures NTP again on its own thread");
        let rx = spawn();
        self.start_boot_ntp(rx);
        self.update_shared_status();
        true
    }

    /// dantesync#134 review — the master's wall is placed: on its fleet line (a restored or new
    /// session it is aligned with), or its boot reading handled with nothing left to decide (no
    /// saved state waiting, no boot step due).
    pub(in crate::controller) fn master_wall_placed(&self) -> bool {
        let ds = &self.date_sync;
        let now = wall_now_ns();
        let on_line = match (ds.authority.as_ref(), ds.d_in_effect(now)) {
            (Some(a), Some(own)) => a.in_effect_ns(now.wrapping_sub(own)) == own,
            _ => false,
        };
        on_line
            || (!self.boot_ntp_pending()
                && ds.restart.pending_restore.is_none()
                && !ds.restart.boot_step_due)
    }

    /// dantesync#134: this node's wall is on the fleet date line already — the master runs its
    /// authority (restored or new), or a follower adopted the master's D.
    fn on_fleet_date_line(&self) -> bool {
        self.date_sync.enabled
            && (self.date_sync.authority.is_some() || self.date_sync.follower.adopted())
    }
}
