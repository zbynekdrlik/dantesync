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
pub type BootNtpResult = std::result::Result<NtpMeasurement, String>;

/// The loop waits this long for the boot reading; after it the node goes on as if the boot NTP
/// had failed (the pre-1.19 "NTP Sync failed" path: the authority starts on the current wall, the
/// periodic NTP takes over).
pub(in crate::controller) const BOOT_NTP_WAIT: Duration = Duration::from_secs(60);

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
        let _ = spawner;
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
            Err(TryRecvError::Empty) => Err(format!(
                "no boot NTP reading within {} s",
                BOOT_NTP_WAIT.as_secs()
            )),
            Err(TryRecvError::Disconnected) => Err("the boot NTP thread ended".to_string()),
        };
        let waited = wait.since.elapsed();
        self.date_sync.restart.boot_ntp = None;
        info!(
            "[NTP] boot reading after {:.1} s (the PTP loop ran meanwhile)",
            waited.as_secs_f64()
        );
        self.apply_boot_ntp(result);
    }

    /// The boot NTP reading: published, then the boot step by the start-up rules (the module doc).
    /// `run_ntp_sync` (the synchronous form) and [`Self::service_boot_ntp`] both end here.
    pub(in crate::controller) fn apply_boot_ntp(&mut self, result: BootNtpResult) {
        let measurement = match result {
            Ok(m) => m,
            Err(e) => {
                warn!("NTP Sync failed: {}", e);
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
        // #68: the boot measurement is a real measurement — publish it.
        let offset_us = if sign > 0 {
            offset.as_micros() as i64
        } else {
            -(offset.as_micros() as i64)
        };
        self.record_ntp_success(offset_us, &measurement);
        // A deferred boot step (a rejected saved state) is THIS step: it does not measure again.
        self.date_sync.restart.boot_step_due = false;
        if self.on_fleet_date_line() {
            info!(
                "[DATE] boot step of {}{:?} NOT taken: this node is on the fleet date line already \
                 (placed at its first PTP window) — the reading is report-only",
                sign_str, offset
            );
            return;
        }
        if offset.as_millis() > BOOT_STEP_THRESHOLD_MS && self.skip_boot_step_for(offset_us) {
            // dantesync#126: the NTP master restores its saved fleet date offset at the first PTP
            // window instead — a restart must not move the fleet date.
            info!(
                "[DATE] boot step of {}{:?} SKIPPED: the saved fleet date offset is restored at the \
                 first PTP window (the date authority corrects the UTC error, coordinated, as if \
                 the master had not restarted)",
                sign_str, offset
            );
            return;
        }
        if self.step_boot_offset(offset, sign) && self.date_sync.core.anchor_ns().is_some() {
            // The phase lock anchored already (the reading came after the first PTP window): D
            // moves with the wall, exactly — the loop sees no disturbance.
            let delta_ns = (offset.as_nanos() as i64).saturating_mul(if sign > 0 { 1 } else { -1 });
            self.date_sync.core.note_step(delta_ns);
            self.reset_ptp_measurement_after_step();
            self.ntp_offset_samples.clear();
            self.ntp_pending_step = None;
        }
    }

    /// dantesync#134: this node's wall is on the fleet date line already — the master runs its
    /// authority (restored or new), or a follower adopted the master's D.
    fn on_fleet_date_line(&self) -> bool {
        self.date_sync.enabled
            && (self.date_sync.authority.is_some() || self.date_sync.follower.adopted())
    }
}
