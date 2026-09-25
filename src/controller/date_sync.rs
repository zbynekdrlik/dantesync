//! dantesync#117 / #88 — the controller's PTP phase lock and fleet date-offset glue.
//!
//! The laws live in pure modules — `crate::ptp_phase_lock` (the PI on `e = (t2 − t1) − D`) and
//! `crate::date_offset` (the authority, the step scheduler, the time-base check) — proven there and
//! end-to-end by `tests/two_clock_bench.rs`. This file only wires them to the controller: which
//! word drives the clock, the NTP master's authority (its NTP reading becomes an announce, never a
//! step), a follower's poll / join / schedule, the coordinated step itself, the anchor lifecycle,
//! and what `/status` publishes. A child module of `controller` so it reaches the controller's
//! private state; all of its state is the one [`DateSync`] field.

use super::*;
use crate::config::{CLOCK_DISCIPLINE_LEGACY, CLOCK_DISCIPLINE_PTP_PHASE_LOCK};
use crate::date_offset::{
    same_time_base, DateAnnounce, DateAuthority, DateFollower, FollowAction, StepKind,
};
use crate::ptp_phase_lock::{AnchorEvent, PhaseLockCore};
use crate::time_server::NoAuthority;

/// A reply from the date-offset authority older than this is not acted on (the poller asks once
/// per second; a stale reply means the master went quiet).
const AUTHORITY_REPLY_MAX_AGE: Duration = Duration::from_secs(5);

/// A follower that has heard no APPLICABLE authority reply (fresh, same grandmaster, same time
/// base) for this long stops following and returns to the local NTP date path — otherwise a
/// silent, re-based or downgraded master would leave it neither following nor stepping, drifting
/// at the grandmaster-vs-UTC rate with `/status` still saying "follower".
const AUTHORITY_LOSS: Duration = Duration::from_secs(30);

/// After a failed date step, announces are not acted on for this long, so a clock that refuses
/// to step is not re-tried (and re-warned) every second.
const STEP_FAILURE_BACKOFF: Duration = Duration::from_secs(10);

fn step_kind_label(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Join => "join",
        StepKind::Coordinated => "coordinated",
        StepKind::Late => "late",
    }
}

/// All phase-lock / date-offset state of one controller.
pub(super) struct DateSync {
    /// False only for `system.clock_discipline = "legacy"`, which keeps every pre-#117 path.
    pub(super) enabled: bool,
    /// The PI on `e = (t2 − t1) − D`; owns the frequency word once PTP-locked.
    pub(super) core: PhaseLockCore,
    /// Raw `t2 − t1` samples of the current window, filled beside the rate servo's `sample_window`
    /// under the same gate and cleared with it.
    pub(super) window: Vec<i64>,
    /// The window median handed from `process_sample_window` to the servo, and its GM time `t1`.
    pub(super) pending_median_ns: Option<i64>,
    pub(super) pending_t1_ns: i64,
    /// GM time of the last phase-lock update: the PI's `dt` is measured in the plant's own time
    /// base, immune to loop scheduling and to this daemon's own wall steps.
    pub(super) last_t1_ns: Option<i64>,
    /// The grandmaster the anchor `D` belongs to (published as the offset's time base).
    pub(super) anchor_gm: Option<[u8; 6]>,
    /// Every box's coordinated-step scheduler (the master's own announces go through it too).
    pub(super) follower: DateFollower,
    /// The fleet date-offset authority — `Some` only on the NTP master, once anchored.
    pub(super) authority: Option<DateAuthority>,
    /// Where a follower reads its master's announce (`UdpAuthorityPoller` in production).
    pub(super) source: Box<dyn DateAuthoritySource>,
    pub(super) last_serial: u64,
    /// The announce this follower last aligned with (published for observability).
    pub(super) last_announce: Option<DateAnnounce>,
    pub(super) last_applicable_reply: Option<Instant>,
    /// A phase-lock window was processed since the last step / PTP outage, so the core's last
    /// error describes the wall as it is NOW (the master's re-alignment needs that).
    pub(super) fresh_window: bool,
    pub(super) step_failed_at: Option<Instant>,
    pub(super) step_bound_ns: i64,
    pub(super) step_lead_ns: i64,
    /// (size ns, wall epoch s, kind) of the last date step this node applied.
    pub(super) last_step: Option<(i64, u64, &'static str)>,
    /// The master's last `UTC − wall` reading (ns), as fed to the authority.
    pub(super) master_utc_error_ns: Option<i64>,
}

impl DateSync {
    /// Read the discipline from the config (logging it, and a warning for a typo or an ignored
    /// `phase_slew`).
    pub(super) fn new(config: &SystemConfig, window_size: usize) -> Self {
        let enabled = !config.legacy_clock_discipline();
        if let Some(bad) = config.unknown_clock_discipline() {
            warn!(
                "system.clock_discipline {:?} is not {:?} or {:?} — using {:?}",
                bad,
                CLOCK_DISCIPLINE_PTP_PHASE_LOCK,
                CLOCK_DISCIPLINE_LEGACY,
                CLOCK_DISCIPLINE_PTP_PHASE_LOCK
            );
        }
        if enabled {
            info!(
                "[PHASE-LOCK] clock discipline: {} — rate AND phase from the Dante PTP grandmaster; \
                 NTP only moves the date, through the fleet date offset (#117/#88)",
                CLOCK_DISCIPLINE_PTP_PHASE_LOCK
            );
            if config.phase_slew.enabled {
                warn!(
                    "[PHASE-LOCK] system.phase_slew.enabled is IGNORED under {} — NTP never steers \
                     the rate (set system.clock_discipline = \"{}\" to get phase_slew back)",
                    CLOCK_DISCIPLINE_PTP_PHASE_LOCK, CLOCK_DISCIPLINE_LEGACY
                );
            }
        } else {
            info!(
                "[PHASE-LOCK] clock discipline: {} — the pre-#117 rate servo + NTP step path",
                CLOCK_DISCIPLINE_LEGACY
            );
        }
        DateSync {
            enabled,
            core: PhaseLockCore::new(),
            window: Vec::with_capacity(window_size),
            pending_median_ns: None,
            pending_t1_ns: 0,
            last_t1_ns: None,
            anchor_gm: None,
            follower: DateFollower::new(),
            authority: None,
            source: Box::new(NoAuthority),
            last_serial: 0,
            last_announce: None,
            last_applicable_reply: None,
            fresh_window: false,
            step_failed_at: None,
            step_bound_ns: config.date_offset.step_bound_ns(),
            step_lead_ns: config.date_offset.step_lead_ns(),
            last_step: None,
            master_utc_error_ns: None,
        }
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #117 — the frequency word for this PTP window. Once PTP-locked the phase lock owns it (a PI
    /// on `e = (t2 − t1) − D`, taken over bumplessly from the rate servo's word); otherwise the
    /// rate servo's `total_correction` applies. The inputs are the PTP window median, the lock
    /// verdict and the rate servo's word — no NTP term exists in this path.
    pub(super) fn phase_lock_word(
        &mut self,
        phase_median_ns: Option<i64>,
        total_correction: f64,
        dt_fallback_s: f64,
    ) -> f64 {
        if !self.date_sync.enabled {
            return total_correction;
        }
        let Some(median_ns) = phase_median_ns else {
            // A window without raw samples (never expected: both windows fill under one gate)
            // must not bump the word back to the rate servo's — hold the phase lock's.
            return if self.date_sync.core.engaged() {
                self.date_sync.core.last_freq_ppm()
            } else {
                total_correction
            };
        };
        // dt in GRANDMASTER time (t1). A GM change jumps it; `on_window` clamps that one window.
        let t1 = self.date_sync.pending_t1_ns;
        let dt = self
            .date_sync
            .last_t1_ns
            .map(|prev| t1.wrapping_sub(prev) as f64 / 1e9)
            .unwrap_or(dt_fallback_s);
        self.date_sync.last_t1_ns = Some(t1);
        let was_engaged = self.date_sync.core.engaged();
        let locked = self.is_locked && !self.ptp_offline;
        let out = self
            .date_sync
            .core
            .on_window(median_ns, locked, total_correction, dt);
        self.handle_phase_anchor_event(out.event);
        self.date_sync.fresh_window = out.error_ns.is_some();
        match out.freq_ppm {
            Some(word) => {
                if !was_engaged {
                    info!(
                        "[PHASE-LOCK] engaged: the frequency word now follows the PTP phase error \
                         (e={:+.1}us, word {:+.3}ppm)",
                        out.error_ns.unwrap_or(0) as f64 / 1_000.0,
                        word
                    );
                }
                // Keep the rate servo's baseline on the learned frequency, so a hand-back (lock
                // loss) is bumpless.
                self.drift_baseline_ppm = self.date_sync.core.integrator_ppm();
                word
            }
            None => {
                if was_engaged {
                    self.drift_baseline_ppm = self.date_sync.core.integrator_ppm();
                    info!(
                        "[PHASE-LOCK] disengaged (PTP lock lost) — the rate servo holds the \
                         learned {:+.3}ppm",
                        self.drift_baseline_ppm
                    );
                }
                total_correction
            }
        }
    }

    /// #88 — inside the backoff after a failed date step.
    fn in_step_backoff(&self) -> bool {
        self.date_sync
            .step_failed_at
            .is_some_and(|t| t.elapsed() < STEP_FAILURE_BACKOFF)
    }

    /// #88 — true while this node is the NTP master acting as the date-offset authority (its only
    /// step threshold is then the authority's bound).
    pub(super) fn date_authority_active(&self) -> bool {
        self.date_sync.enabled
            && self.ntp_server_mode
            && self.date_sync.authority.is_some()
            && !self.ptp_offline
    }

    /// #117 / #88 — publish the discipline, the phase lock and the fleet date offset. `D` in
    /// effect and a scheduled step are written together (one status write), so the 31900
    /// extension never counts a step twice; nothing is published while a re-anchor is pending (`D`
    /// would still be in the OLD time base) — and the time base is named by the ANCHOR's
    /// grandmaster, never the one merely heard.
    pub(super) fn publish_date_status(&self, status: &mut SyncStatus) {
        let ds = &self.date_sync;
        status.clock_discipline = if ds.enabled {
            CLOCK_DISCIPLINE_PTP_PHASE_LOCK
        } else {
            CLOCK_DISCIPLINE_LEGACY
        }
        .to_string();
        status.rate_source = if self.phase_slew.is_some() {
            "ptp+ntp"
        } else {
            "ptp"
        }
        .to_string();
        status.ptp_phase_locked = ds.core.engaged();
        status.ptp_phase_error_us = if ds.enabled {
            ds.core.last_error_ns().map(|e| e as f64 / 1_000.0)
        } else {
            None
        };

        let anchor = if ds.enabled && !ds.core.rebase_pending() {
            ds.core.anchor_ns()
        } else {
            None
        };
        let now_wall = wall_now_ns();
        status.date_offset_ns = anchor;
        status.date_offset_gm_uuid = anchor.and(ds.anchor_gm);
        status.date_authority = match anchor {
            None => String::new(),
            Some(_) if ds.authority.is_some() => "master".to_string(),
            Some(_) if ds.follower.adopted() => "follower".to_string(),
            Some(_) => "local".to_string(),
        };
        let published = match ds.authority.as_ref() {
            Some(a) => Some(a.announce()),
            None => ds.last_announce,
        };
        status.date_offset_seq = anchor.and(published.map(|p| p.seq));
        status.date_offset_effective_ptp_ns = anchor.and(published.map(|p| p.effective_ptp_ns));
        // The master publishes exactly its authority's announce: D in effect on its own wall
        // (the anchor) plus the difference to the announced D. While a step is pending that is
        // the step; once its instant has passed but before this loop applies it, or while the
        // master is off the fleet line (its own PTP outage, a failed step), it is the correction
        // back to the fleet D — so a follower always reads the FLEET D, never the master's own.
        match (ds.authority.as_ref(), anchor) {
            (Some(a), Some(d)) => {
                let ann = a.announce();
                let delta = ann.date_offset_ns.wrapping_sub(d);
                status.date_step_pending_ns = (delta != 0).then_some(delta);
                status.date_step_due_in_ms = status.date_step_pending_ns.map(|_| {
                    ann.effective_ptp_ns.wrapping_sub(now_wall.wrapping_sub(d)) / 1_000_000
                });
            }
            _ => {
                status.date_step_pending_ns = ds.follower.pending().map(|p| p.delta_ns);
                status.date_step_due_in_ms =
                    ds.follower.time_to_due_ns(now_wall).map(|n| n / 1_000_000);
            }
        }
        let master = ds.authority.is_some();
        status.date_offset_error_ms = if master {
            ds.master_utc_error_ns.map(|e| e as f64 / 1e6)
        } else {
            None
        };
        status.date_step_bound_ms = if master {
            Some(ds.step_bound_ns as f64 / 1e6)
        } else {
            None
        };
        status.last_date_step_ns = ds.last_step.map(|s| s.0);
        status.last_date_step_ts = ds.last_step.map(|s| s.1);
        status.last_date_step_kind = ds.last_step.map(|s| s.2.to_string()).unwrap_or_default();
        status.date_steps_late = ds.follower.late_steps();
    }

    /// #117 — the LOCAL date path stepped the wall by `delta_ns` (the NTP step path: no authority
    /// heard, or this box's own PTP is offline). `D` moves with the wall so the phase lock sees no
    /// disturbance. The FLEET date offset is never moved by it: on the master the authority keeps
    /// the fleet D (still disciplined to UTC, see `ntp_under_date_authority`), and the master
    /// re-aligns its own wall to it once PTP is back (`realign_master_to_fleet`) — a single box's
    /// fault never reaches the fleet as a step.
    pub(super) fn note_local_date_step(&mut self, delta_ns: i64) {
        self.date_sync.core.note_step(delta_ns);
        // Only the MASTER drops its own scheduled step (its local step already tracks UTC, and it
        // re-aligns to the fleet later). A follower keeps it: its NTP source is the master, whose
        // wall has not stepped yet, so its local step never covered the fleet's step.
        if self.ntp_server_mode {
            self.date_sync.follower.cancel_pending();
        }
        self.date_sync.last_step =
            Some((delta_ns, (wall_now_ns() / 1_000_000_000) as u64, "local"));
    }

    /// #88 — the NTP master back on the fleet line: once its own PTP is online and the phase lock
    /// engaged, a master whose `D` differs from the authority's (it ran the local NTP path through
    /// its own PTP outage, re-anchored on re-engagement, or a coordinated step failed on it) steps
    /// its OWN wall to the fleet D — one Join step, never a change of the fleet D. Nothing happens
    /// while a coordinated step is pending (the scheduler owns that), during the failure backoff,
    /// or mid re-anchor.
    fn realign_master_to_fleet(&mut self) {
        if !self.date_authority_active()
            || !self.date_sync.core.engaged()
            || self.date_sync.core.rebase_pending()
        {
            return;
        }
        if self.in_step_backoff() {
            return;
        }
        let (Some(anchor), Some(a)) = (
            self.date_sync.core.anchor_ns(),
            self.date_sync.authority.as_ref(),
        ) else {
            return;
        };
        let now_ptp = wall_now_ns().wrapping_sub(anchor);
        if a.pending_step_ns(now_ptp).is_some() || self.date_sync.follower.pending().is_some() {
            return;
        }
        let fleet = a.in_effect_ns(now_ptp);
        if fleet == anchor {
            return;
        }
        // The wall must land ON the fleet line, so the step also removes the phase error the
        // outage left (the clock free-ran with no PTP windows): it is measured, so wait for a
        // window taken after PTP came back.
        if !self.date_sync.fresh_window {
            return;
        }
        let e = self.date_sync.core.last_error_ns().unwrap_or(0);
        let delta = fleet.wrapping_sub(anchor).wrapping_sub(e);
        let seq = a.seq();
        if delta.abs() > crate::date_offset::ABSORB_TOLERANCE_NS {
            warn!(
                "[DATE] the master is {:+}us off the fleet date offset (its own PTP outage, a \
                 re-anchor or a failed step) — stepping its OWN wall back to the fleet line",
                delta / 1_000
            );
            self.apply_date_step(delta, StepKind::Join, seq);
            if self.date_sync.step_failed_at.is_some() {
                return;
            }
        }
        self.date_sync.core.set_anchor(fleet);
    }

    /// #117 / #88 — the NTP reading under the phase lock. Returns true when it was fully handled
    /// here (the caller must NOT run the NTP step path):
    ///
    /// - the NTP master feeds the FLEET line's UTC error to the date-offset authority, which may
    ///   announce a coordinated step (it never steps here); while it has no PTP itself it still
    ///   feeds the authority and returns false, so its OWN wall keeps the local NTP path;
    /// - a follower aligned with the authority only reports the reading (its date moves only at
    ///   announced instants).
    ///
    /// Everything else (legacy discipline, not anchored yet, PTP offline, no authority heard or
    /// the authority lost) returns false and keeps the existing NTP step path — the local date
    /// fallback.
    pub(super) fn ntp_under_date_authority(&mut self, offset_us: i64) -> bool {
        if !self.date_sync.enabled {
            return false;
        }
        let Some(anchor) = self.date_sync.core.anchor_ns() else {
            return false;
        };
        if self.ntp_server_mode {
            self.ensure_date_authority();
            let Some(fleet) = self
                .date_sync
                .authority
                .as_ref()
                .map(|a| a.in_effect_ns(wall_now_ns().wrapping_sub(anchor)))
            else {
                return false;
            };
            let now_wall = wall_now_ns();
            let now_ptp = now_wall.wrapping_sub(anchor);
            // The authority owns the FLEET line, so it is fed the fleet line's UTC error: this
            // master's reading plus how far its own wall is off that line (`anchor − fleet`). On
            // the line that is the reading itself; through the master's own PTP outage (its wall
            // on the local NTP path) or a failed step it keeps the FLEET on UTC — the only error
            // left is the master's free-run drift, ≪ the step bound.
            let fleet_err = offset_us
                .saturating_mul(1_000)
                .wrapping_add(anchor.wrapping_sub(fleet));
            self.date_sync.master_utc_error_ns = Some(fleet_err);
            let on_line = anchor == fleet && !self.ptp_offline && !self.in_step_backoff();
            if !self.ptp_offline {
                // Log-surface contract: every NTP cycle keeps the exact `[NTP] offset:{:+}us`
                // prefix the camera-box freshness gates parse (offline, the NTP step path logs it).
                info!(
                    "[NTP] offset:{:+}us (date authority, fleet line {:+}us, step bound {}us)",
                    offset_us,
                    fleet_err / 1_000,
                    self.date_sync.step_bound_ns / 1_000
                );
            }
            let announced = self
                .date_sync
                .authority
                .as_mut()
                .and_then(|a| a.on_utc_error(fleet_err, now_ptp));
            if let Some(ann) = announced {
                info!(
                    "[DATE] AUTHORITY: the fleet line is {:+}us off UTC (> {}us) — announcing a fleet \
                     date step of {:+}us at PTP {} (in {} ms), seq {}{}",
                    fleet_err / 1_000,
                    self.date_sync.step_bound_ns / 1_000,
                    ann.date_offset_ns.wrapping_sub(fleet) / 1_000,
                    ann.effective_ptp_ns,
                    ann.effective_ptp_ns.wrapping_sub(now_ptp) / 1_000_000,
                    ann.seq,
                    if on_line {
                        ""
                    } else {
                        " (this master is off the fleet line: it re-aligns afterwards)"
                    }
                );
                if on_line {
                    let act = self.date_sync.follower.on_announce(ann, anchor, now_wall);
                    debug!("[DATE] master's own scheduler: {:?}", act);
                }
            }
            if self.ptp_offline {
                // Its OWN wall keeps the local NTP step path while it has no PTP.
                return false;
            }
            // The NTP step path is bypassed: nothing pending, nothing starved.
            self.ntp_pending_step = None;
            self.ntp_server_checks_since_step = 0;
            self.update_shared_status();
            return true;
        }
        if self.ptp_offline {
            return false;
        }
        if self.date_sync.follower.adopted() {
            info!(
                "[NTP] offset:{:+}us (following the fleet date offset — no NTP step)",
                offset_us
            );
            self.ntp_pending_step = None;
            return true;
        }
        false
    }

    /// #88 — make the NTP master the fleet date-offset authority once it is anchored, and align
    /// its own scheduler with itself (so its announces are scheduled like everyone's).
    pub(super) fn ensure_date_authority(&mut self) {
        if !self.date_sync.enabled || !self.ntp_server_mode || self.date_sync.authority.is_some() {
            return;
        }
        let Some(anchor) = self.date_sync.core.anchor_ns() else {
            return;
        };
        let now_wall = wall_now_ns();
        let authority = DateAuthority::new(
            anchor,
            now_wall.wrapping_sub(anchor),
            self.date_sync.step_bound_ns,
            self.date_sync.step_lead_ns,
        );
        let act = self
            .date_sync
            .follower
            .on_announce(authority.announce(), anchor, now_wall);
        debug!("[DATE] master aligned with its own authority: {:?}", act);
        info!(
            "[DATE] this NTP master is the fleet DATE-OFFSET AUTHORITY: D={}ns, step bound {} ms, \
             announce lead {} s — clients step together at the announced PTP instant",
            anchor,
            self.date_sync.step_bound_ns / 1_000_000,
            self.date_sync.step_lead_ns / 1_000_000_000
        );
        self.date_sync.authority = Some(authority);
    }

    /// #117 — react to the phase lock's anchor lifecycle.
    pub(super) fn handle_phase_anchor_event(&mut self, event: AnchorEvent) {
        let gm_label = self
            .current_gm_uuid
            .as_ref()
            .map(format_mac)
            .unwrap_or_else(|| "?".to_string());
        match event {
            AnchorEvent::None => {}
            AnchorEvent::Anchored { anchor_ns } => {
                info!(
                    "[PHASE-LOCK] anchored: wall = PTP time + D, D={}ns (grandmaster {})",
                    anchor_ns, gm_label
                );
                self.date_sync.anchor_gm = self.current_gm_uuid;
                self.ensure_date_authority();
            }
            AnchorEvent::Realigned { old_ns, new_ns } => {
                // Same time base, this box's wall wandered: the fleet D stays. A follower re-joins
                // at its next poll; the master steps its own wall back (`realign_master_to_fleet`).
                info!(
                    "[PHASE-LOCK] re-engaged {:+}us off D — re-anchored on the current offset; \
                     the date layer re-aligns the wall to the fleet with one step",
                    new_ns.wrapping_sub(old_ns) / 1_000
                );
            }
            AnchorEvent::Rebased { old_ns, new_ns } => {
                info!(
                    "[PHASE-LOCK] re-anchored, wall continuous (no step): D {} -> {} ns \
                     (grandmaster {})",
                    old_ns, new_ns, gm_label
                );
                self.date_sync.anchor_gm = self.current_gm_uuid;
                if let Some(a) = self.date_sync.authority.as_mut() {
                    // "now" in the OLD base: the wall did not move, the base did. The FLEET line is
                    // shifted by the observed base shift — not set to this master's own anchor,
                    // which may be off the fleet line (its own PTP outage, a failed step): folding
                    // that offset in would reach every follower as a step.
                    let now_ptp_old = wall_now_ns().wrapping_sub(old_ns);
                    let fleet_old = a.in_effect_ns(now_ptp_old);
                    let fleet_new = fleet_old.wrapping_add(new_ns.wrapping_sub(old_ns));
                    let ann = a.rebase(fleet_new, now_ptp_old);
                    info!(
                        "[DATE] authority rebased onto the new time base (seq {})",
                        ann.seq
                    );
                }
            }
        }
    }

    /// #88 — every loop iteration: apply a coordinated step whose instant has come, and (a
    /// follower) act once on each new, APPLICABLE announce from the master.
    pub(super) fn service_date_offset(&mut self) {
        if !self.date_sync.enabled || self.date_sync.core.anchor_ns().is_none() {
            return;
        }
        if self.ptp_offline {
            self.date_sync.fresh_window = false;
            if self.date_sync.core.engaged() {
                // No PTP, no phase lock: hand the learned frequency back to the rate servo, and
                // let the next locked window re-engage (and re-align if the wall free-ran).
                self.date_sync.core.disengage();
                self.drift_baseline_ppm = self.date_sync.core.integrator_ppm();
                info!(
                    "[PHASE-LOCK] disengaged (PTP offline) — holding {:+.3}ppm",
                    self.drift_baseline_ppm
                );
            }
        }
        if let Some(due) = self.date_sync.follower.due(wall_now_ns()) {
            self.apply_date_step(due.delta_ns, StepKind::Coordinated, due.seq);
        }
        if self.ntp_server_mode {
            self.ensure_date_authority();
            self.realign_master_to_fleet();
            return;
        }
        let lost = match self.date_sync.last_applicable_reply {
            None => true,
            Some(t) => t.elapsed() > AUTHORITY_LOSS,
        };
        if lost && self.date_sync.follower.adopted() {
            warn!(
                "[DATE] no applicable date-offset authority reply for {}s — back to the local NTP \
                 date path until the master is heard again",
                AUTHORITY_LOSS.as_secs()
            );
            self.date_sync.follower.forget();
            self.date_sync.last_announce = None;
        }
        if self.in_step_backoff() {
            return;
        }
        let Some(reply) = self.date_sync.source.latest() else {
            return;
        };
        if reply.serial == self.date_sync.last_serial {
            return;
        }
        self.date_sync.last_serial = reply.serial;
        if reply.received.elapsed() > AUTHORITY_REPLY_MAX_AGE {
            return;
        }
        let Some(ext) = reply.ext.filter(|e| e.authority) else {
            return;
        };
        // D belongs to the master's PTP time base. Adopt it only when this box is in that same
        // base: PTP online, no re-anchor pending, the same grandmaster as OUR anchor, and — the
        // decisive check, which also catches a grandmaster that rebooted under the same UUID —
        // both nodes reading the same PTP "now" (`same_time_base`).
        if self.ptp_offline || self.date_sync.core.rebase_pending() {
            return;
        }
        let Some(anchor) = self.date_sync.core.anchor_ns() else {
            return;
        };
        if Some(ext.gm_uuid) != self.date_sync.anchor_gm
            || !same_time_base(ext.now_ptp_ns, reply.received_wall_ns, anchor)
        {
            debug!(
                "[DATE] authority announce seq {} is in another PTP time base (its GM {:?}, ours \
                 {:?}) — not applicable here",
                ext.announce.seq, ext.gm_uuid, self.date_sync.anchor_gm
            );
            return;
        }
        self.date_sync.last_applicable_reply = Some(Instant::now());
        self.date_sync.last_announce = Some(ext.announce);
        let now_wall = wall_now_ns();
        let first = !self.date_sync.follower.adopted();
        match self
            .date_sync
            .follower
            .on_announce(ext.announce, anchor, now_wall)
        {
            FollowAction::None => {}
            FollowAction::Absorb { new_anchor_ns } => {
                self.date_sync.core.set_anchor(new_anchor_ns);
                if first {
                    info!(
                        "[DATE] aligned with the fleet date offset (seq {}): D adopted, {:+}ns \
                         inside the absorb tolerance — no step",
                        ext.announce.seq,
                        new_anchor_ns.wrapping_sub(anchor)
                    );
                }
            }
            FollowAction::Scheduled {
                delta_ns,
                effective_wall_ns,
            } => info!(
                "[DATE] coordinated date step {:+}us scheduled (seq {}) in {} ms",
                delta_ns / 1_000,
                ext.announce.seq,
                effective_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
            FollowAction::Step { delta_ns, kind } => {
                self.apply_date_step(delta_ns, kind, ext.announce.seq)
            }
        }
    }

    /// #88 — step the wall by `delta_ns` for the fleet date offset and move `D` with it.
    pub(super) fn apply_date_step(&mut self, delta_ns: i64, kind: StepKind, seq: u32) {
        if delta_ns == 0 {
            return;
        }
        let label = step_kind_label(kind);
        let dur = Duration::from_nanos(delta_ns.unsigned_abs());
        let sign: i8 = if delta_ns > 0 { 1 } else { -1 };
        if let Err(e) = self.clock.step_clock(dur, sign) {
            // D is NOT moved and the fleet D is untouched. A follower re-joins after the backoff; the
            // master publishes the fleet D regardless and steps its own wall to it after the
            // backoff (`realign_master_to_fleet`), feeding no UTC reading meanwhile.
            warn!(
                "[DATE] {} date step {:+}us (seq {}) FAILED: {} — retrying the alignment in {}s",
                label,
                delta_ns / 1_000,
                seq,
                e,
                STEP_FAILURE_BACKOFF.as_secs()
            );
            self.date_sync.step_failed_at = Some(Instant::now());
            self.update_shared_status();
            return;
        }
        self.date_sync.step_failed_at = None;
        self.date_sync.core.note_step(delta_ns);
        // #68 on the master: after a COORDINATED step publish the UTC error that REMAINS, not the
        // one just stepped away (a re-alignment Join moves only this master onto the fleet line).
        if kind == StepKind::Coordinated && self.date_sync.authority.is_some() {
            if let Some(err) = self.date_sync.master_utc_error_ns {
                let residual = err.wrapping_sub(delta_ns);
                self.date_sync.master_utc_error_ns = Some(residual);
                self.publish_post_step_residual(residual / 1_000);
            }
        }
        self.reset_ptp_measurement_after_step();
        self.ntp_offset_samples.clear();
        self.ntp_pending_step = None;
        self.date_sync.last_step = Some((delta_ns, (wall_now_ns() / 1_000_000_000) as u64, label));
        if kind == StepKind::Late {
            warn!(
                "[DATE] LATE date step {:+}us (seq {}): the announce was first heard after its \
                 instant — this box stepped out of sync with the fleet",
                delta_ns / 1_000,
                seq
            );
        } else {
            info!(
                "[DATE] stepped {:+}us ({}, seq {})",
                delta_ns / 1_000,
                label,
                seq
            );
        }
        // #91: a date step is this node's NTP-driven step; count it for the storm alarm.
        self.record_ntp_step_and_check_storm();
        self.update_shared_status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockSystemClock;
    use crate::traits::{MockNtpSource, MockPtpNetwork};

    fn one_offset(us: i64, sign: i8) -> crate::ntp::NtpMeasurement {
        crate::ntp::NtpMeasurement {
            offset: Duration::from_micros(us.unsigned_abs()),
            sign,
            spread_us: 40,
            sample_count: 3,
            pcap_active: false,
        }
    }

    // ========================================================================
    // PTP PHASE LOCK + FLEET DATE OFFSET WIRING (dantesync#117 / #88)
    // ========================================================================
    //
    // The laws themselves (the PI, the authority, the scheduler) are proven in their own modules
    // and end-to-end by `tests/two_clock_bench.rs`; these tests pin the CONTROLLER wiring: who
    // owns the frequency word, which path may step the clock, and what /status publishes.

    struct ScriptedAuthority(Arc<std::sync::Mutex<Option<crate::time_server::AuthorityReply>>>);

    impl crate::time_server::DateAuthoritySource for ScriptedAuthority {
        fn latest(&self) -> Option<crate::time_server::AuthorityReply> {
            *self.0.lock().expect("scripted authority lock")
        }
    }

    const PL_GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c];
    const PL_PTP_NOW_NS: i64 = 10_000_000_000;

    fn phase_lock_config() -> SystemConfig {
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        config
    }

    /// A controller anchored on the phase lock (first lock) at `D = wall − 10 s`, so its view of
    /// the grandmaster's PTP time is 10 s. `master` configures NTP server mode FIRST, so the
    /// anchor makes it the date-offset authority.
    fn anchored_controller(
        mut clock: MockSystemClock,
        ntp: MockNtpSource,
        master: bool,
    ) -> (
        PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
        i64,
    ) {
        clock.expect_adjust_frequency().returning(|_| Ok(()));
        let mut c = PtpController::new(
            clock,
            MockPtpNetwork::new(),
            ntp,
            Arc::new(RwLock::new(SyncStatus::default())),
            phase_lock_config(),
        );
        if master {
            c.configure_ntp_server_mode(100_000);
        }
        c.current_gm_uuid = Some(PL_GM);
        c.is_locked = true;
        let d = wall_now_ns() - PL_PTP_NOW_NS;
        c.date_sync.pending_median_ns = Some(d);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS;
        c.apply_self_tuning_servo(0.0);
        assert_eq!(
            c.date_sync.core.anchor_ns(),
            Some(d),
            "the first lock anchors D"
        );
        assert!(c.date_sync.core.engaged());
        (c, d)
    }

    fn authority_reply(
        serial: u64,
        gm: [u8; 6],
        date_offset_ns: i64,
        effective_ptp_ns: i64,
        seq: u32,
    ) -> crate::time_server::AuthorityReply {
        authority_reply_in_effect(
            serial,
            gm,
            date_offset_ns,
            effective_ptp_ns,
            seq,
            date_offset_ns,
        )
    }

    /// A reply whose published D may carry a pending step; `in_effect_ns` is the master's D in
    /// effect (what its PTP "now" is taken from). Both walls read "now", so a D in effect in this
    /// box's base passes the time-base check and one days away fails.
    fn authority_reply_in_effect(
        serial: u64,
        gm: [u8; 6],
        date_offset_ns: i64,
        effective_ptp_ns: i64,
        seq: u32,
        in_effect_ns: i64,
    ) -> crate::time_server::AuthorityReply {
        let now = wall_now_ns();
        crate::time_server::AuthorityReply {
            serial,
            gm_uuid: Some(gm),
            is_locked: true,
            received_wall_ns: now,
            ext: Some(crate::date_offset::DateExtension {
                version: crate::date_offset::EXT_VERSION,
                authority: true,
                announce: DateAnnounce {
                    date_offset_ns,
                    effective_ptp_ns,
                    seq,
                },
                gm_uuid: gm,
                now_ptp_ns: now - in_effect_ns,
            }),
            received: Instant::now(),
        }
    }

    fn with_authority(
        c: &mut PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
    ) -> Arc<std::sync::Mutex<Option<crate::time_server::AuthorityReply>>> {
        let slot = Arc::new(std::sync::Mutex::new(None));
        c.set_date_authority_source(Box::new(ScriptedAuthority(slot.clone())));
        slot
    }

    #[test]
    fn the_phase_lock_is_the_default_and_phase_slew_survives_only_under_legacy_117() {
        let mut config = phase_lock_config();
        config.phase_slew.enabled = true;
        let c = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            config.clone(),
        );
        assert!(c.phase_lock_enabled());
        assert!(
            c.phase_slew.is_none(),
            "NTP must never steer the rate under the phase lock"
        );

        config.clock_discipline = CLOCK_DISCIPLINE_LEGACY.to_string();
        let legacy = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            config,
        );
        assert!(!legacy.phase_lock_enabled());
        assert!(legacy.phase_slew.is_some(), "legacy keeps phase_slew");
    }

    #[test]
    fn once_locked_the_phase_lock_owns_the_frequency_word_117() {
        let captured = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
        let cap = captured.clone();
        let mut clock = MockSystemClock::new();
        clock.expect_adjust_frequency().returning(move |factor| {
            cap.lock().expect("cap").push((factor - 1.0) * 1e6);
            Ok(())
        });
        let mut c = PtpController::new(
            clock,
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            phase_lock_config(),
        );
        c.current_gm_uuid = Some(PL_GM);
        c.is_locked = true;
        c.drift_baseline_ppm = 12.0;
        let d = 1_790_000_000_000_000_000_i64;
        c.date_sync.pending_median_ns = Some(d);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS;
        c.apply_self_tuning_servo(0.0);
        // Bumpless: the first word is the rate servo's (12 ppm, rate 0).
        // A +500 µs phase error half a second later (grandmaster time) pulls the word DOWN.
        c.date_sync.pending_median_ns = Some(d + 500_000);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + 500_000_000;
        c.apply_self_tuning_servo(0.0);
        let words = captured.lock().expect("cap").clone();
        assert!(
            (words[0] - 12.0).abs() < 1e-6,
            "bumpless hand-over, got {}",
            words[0]
        );
        let expect = 12.0
            - crate::ptp_phase_lock::K_I_PER_S2 * 500.0 * 0.5
            - crate::ptp_phase_lock::K_P_PER_S * 500.0;
        assert!(
            (words[1] - expect).abs() < 1e-6,
            "the PI word from the PTP error, got {} want {}",
            words[1],
            expect
        );
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert!(st.ptp_phase_locked);
        assert_eq!(st.ptp_phase_error_us, Some(500.0));
        assert_eq!(st.clock_discipline, "ptp_phase_lock");
        assert_eq!(st.rate_source, "ptp");
        assert!(
            (st.drift_ppm - expect).abs() < 1e-6,
            "drift_ppm is the applied word"
        );
    }

    #[test]
    fn a_follower_joins_the_masters_offset_with_one_step_and_acts_once_per_reply_88() {
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .withf(|d, sign| *d == Duration::from_micros(3_000) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d + 3_000_000, 5_000_000_000, 7));
        c.service_date_offset();
        assert_eq!(
            c.date_sync.core.anchor_ns(),
            Some(d + 3_000_000),
            "D moved with the wall"
        );
        c.service_date_offset(); // the same reply again: acted on once only
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.date_authority, "follower");
        assert_eq!(st.last_date_step_kind, "join");
        assert_eq!(st.last_date_step_ns, Some(3_000_000));
        assert_eq!(st.date_offset_ns, Some(d + 3_000_000));
        assert_eq!(st.date_offset_seq, Some(7));
    }

    #[test]
    fn a_follower_applies_an_announced_step_only_at_its_instant_88() {
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .withf(|d, sign| *d == Duration::from_millis(60) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        // Aligned already (same D): adopted with no step.
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 1_000_000_000, 3));
        c.service_date_offset();
        // The master announces +60 ms, 1 s ahead of now (in PTP time).
        let now_ptp = wall_now_ns() - d;
        *slot.lock().unwrap() = Some(authority_reply(
            2,
            PL_GM,
            d + 60_000_000,
            now_ptp + 1_000_000_000,
            4,
        ));
        c.service_date_offset();
        c.update_shared_status();
        {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(
                st.date_step_pending_ns,
                Some(60_000_000),
                "scheduled, not applied"
            );
        }
        c.service_date_offset(); // still before the instant: nothing
        std::thread::sleep(Duration::from_millis(1_200));
        c.service_date_offset(); // at/after the instant: the step lands
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d + 60_000_000));
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.last_date_step_kind, "coordinated");
        assert_eq!(st.date_step_pending_ns, None);
        assert_eq!(st.date_steps_late, 0);
    }

    #[test]
    fn a_follower_ignores_an_offset_from_another_grandmasters_time_base_88() {
        // No step_clock expectation: any step panics the mock.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        let other_gm = [0x00, 0x1d, 0xc1, 0x99, 0x99, 0x99];
        *slot.lock().unwrap() = Some(authority_reply(1, other_gm, d + 3_000_000, 1, 1));
        c.service_date_offset();
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
        c.update_shared_status();
        let st = c.get_status_shared();
        assert_eq!(st.read().expect("status").date_authority, "local");
    }

    #[test]
    fn a_follower_never_steps_on_its_own_ntp_reading_88() {
        let mut ntp = MockNtpSource::new();
        ntp.expect_get_offset()
            .returning(|| Ok(one_offset(5_000, 1)));
        // No step_clock expectation: an NTP-driven step would panic the mock.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), ntp, false);
        let slot = with_authority(&mut c);
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 1_000_000_000, 3));
        c.service_date_offset();
        for _ in 0..3 {
            c.last_ntp_check = Instant::now() - Duration::from_secs(120);
            c.check_ntp_utc_tracking();
        }
        let st = c.get_status_shared();
        assert_eq!(
            st.read().expect("status").ntp_offset_us,
            5_000,
            "the reading is still published — it is a health signal now"
        );
    }

    #[test]
    fn the_master_announces_a_utc_error_past_the_bound_instead_of_stepping_88() {
        let mut ntp = MockNtpSource::new();
        ntp.expect_get_offset()
            .returning(|| Ok(one_offset(60_000, 1)));
        // No step_clock expectation: the master never steps at NTP time under the authority.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), ntp, true);
        assert!(
            c.date_sync.authority.is_some(),
            "the anchored master is the authority"
        );
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(
                st.date_step_pending_ns, None,
                "one reading is never trusted"
            );
            assert_eq!(st.date_authority, "master");
            assert_eq!(
                st.ntp_deadband_us,
                Some(50_000),
                "graded on the authority bound"
            );
        }
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.date_step_pending_ns, Some(60_000_000));
        let due = st.date_step_due_in_ms.expect("scheduled");
        assert!(
            (4_000..=5_000).contains(&due),
            "announced 5 s ahead, due in {due} ms"
        );
        assert_eq!(st.date_offset_seq, Some(2));
        assert_eq!(
            st.date_offset_ns,
            Some(d),
            "still in effect until the instant"
        );
        assert_eq!(st.date_offset_error_ms, Some(60.0));
    }

    #[test]
    fn a_grandmaster_change_re_anchors_and_rebases_the_authority_without_a_step_117() {
        // No step_clock expectation: a re-anchor must never step the wall.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), true);
        let seq_before = c.date_sync.authority.as_ref().unwrap().seq();
        let new_gm = [0x00, 0x1d, 0xc1, 0x44, 0x55, 0x66];
        c.current_gm_uuid = Some(new_gm);
        c.date_sync.core.request_rebase();
        // The new grandmaster's uptime is 5 days behind: t2 − t1 grows by 5 days.
        let five_days: i64 = 5 * 86_400 * 1_000_000_000;
        c.date_sync.pending_median_ns = Some(d + five_days);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS - five_days;
        c.apply_self_tuning_servo(0.0);
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d + five_days));
        assert_eq!(c.date_sync.anchor_gm, Some(new_gm));
        assert_eq!(
            c.date_sync.authority.as_ref().unwrap().seq(),
            seq_before + 1
        );
        assert_eq!(c.date_sync.core.last_error_ns(), Some(0));
    }

    #[test]
    fn a_grandmaster_that_rebooted_under_the_same_uuid_is_never_adopted_88() {
        // The master still publishes D in the OLD base (uptime 3 days) while this box already
        // re-anchored on the rebooted grandmaster (uptime seconds): same UUID, days apart. It must
        // be skipped — the step it implies is ~3 days. No step_clock expectation: a step panics.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        let three_days: i64 = 3 * 86_400 * 1_000_000_000;
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d - three_days, 1, 9));
        c.service_date_offset();
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
        assert!(!c.date_sync.follower.adopted());
    }

    #[test]
    fn nothing_is_published_or_adopted_while_a_re_anchor_is_pending_88() {
        let (mut m, _) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), true);
        m.date_sync.core.request_rebase();
        m.update_shared_status();
        {
            let st = m.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(
                st.date_offset_ns, None,
                "D is in the old base until the re-anchor"
            );
            assert_eq!(st.date_offset_gm_uuid, None);
        }
        // The follower side too: a reply is not acted on mid re-anchor.
        let (mut f, fd) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
        let slot = with_authority(&mut f);
        f.date_sync.core.request_rebase();
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, fd + 3_000_000, 1, 2));
        f.service_date_offset();
        assert!(!f.date_sync.follower.adopted());
    }

    #[test]
    fn a_follower_that_loses_the_authority_returns_to_the_local_ntp_path_88() {
        let mut ntp = MockNtpSource::new();
        ntp.expect_get_offset()
            .returning(|| Ok(one_offset(5_000, 1)));
        let mut clock = MockSystemClock::new();
        // Back on the local path, a real 5 ms NTP error is stepped again (2 agreeing readings).
        clock.expect_step_clock().times(1).returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, ntp, false);
        let slot = with_authority(&mut c);
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 1_000_000_000, 3));
        c.service_date_offset();
        assert!(c.date_sync.follower.adopted());
        // The master goes silent for longer than the loss window.
        c.date_sync.last_applicable_reply =
            Some(Instant::now() - AUTHORITY_LOSS - Duration::from_secs(1));
        c.service_date_offset();
        assert!(
            !c.date_sync.follower.adopted(),
            "no longer following a silent master"
        );
        for _ in 0..2 {
            c.last_ntp_check = Instant::now() - Duration::from_secs(120);
            c.check_ntp_utc_tracking();
        }
        assert_eq!(
            c.date_sync.core.anchor_ns(),
            Some(d + 5_000_000),
            "D moved with the local step"
        );
        c.update_shared_status();
        let st = c.get_status_shared();
        assert_eq!(st.read().expect("status").date_authority, "local");
    }

    #[test]
    fn a_follower_schedules_a_multi_second_announced_step_88() {
        // The master booted seconds off UTC: its first announce is +3 s. The PUBLISHED D carries
        // the pending step, but the time-base check uses the master's D in effect, so the
        // follower schedules it like any other coordinated step (no refusal, no late step).
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 1_000_000_000, 1));
        c.service_date_offset();
        let now_ptp = wall_now_ns() - d;
        *slot.lock().unwrap() = Some(authority_reply_in_effect(
            2,
            PL_GM,
            d + 3_000_000_000,
            now_ptp + 5_000_000_000,
            2,
            d,
        ));
        c.service_date_offset();
        c.update_shared_status();
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.date_step_pending_ns, Some(3_000_000_000));
        assert_eq!(st.date_steps_late, 0);
    }

    #[test]
    fn a_master_local_step_never_moves_the_fleet_offset_and_the_master_realigns_88() {
        // ONLY the master lost PTP and its local NTP path stepped −250 µs. The fleet D must not
        // move (followers hold it); once PTP is back the master steps its OWN wall back to it.
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .withf(|dur, sign| *dur == Duration::from_micros(250) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), true);
        c.note_local_date_step(-250_000);
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 250_000));
        let now_ptp = wall_now_ns() - d;
        assert_eq!(
            c.date_sync
                .authority
                .as_ref()
                .unwrap()
                .in_effect_ns(now_ptp),
            d,
            "the fleet D is untouched"
        );
        c.update_shared_status();
        {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert_eq!(
                st.date_offset_ns,
                Some(d - 250_000),
                "the master's own D in effect"
            );
            assert_eq!(
                st.date_step_pending_ns,
                Some(250_000),
                "but it publishes the FLEET D (own D + the way back)"
            );
        }
        // PTP online and engaged: the master re-aligns its own wall (+250 µs, a Join step).
        c.service_date_offset();
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
    }

    #[test]
    fn a_failed_step_on_the_master_keeps_the_fleet_offset_and_retries_after_the_backoff_88() {
        let mut seq_calls = mockall::Sequence::new();
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .in_sequence(&mut seq_calls)
            .returning(|_, _| Err(anyhow::anyhow!("clock refused")));
        clock
            .expect_step_clock()
            .times(1)
            .in_sequence(&mut seq_calls)
            .withf(|dur, sign| *dur == Duration::from_micros(250) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), true);
        // The master is 250 µs off the fleet line (a local step during its own PTP outage).
        c.note_local_date_step(-250_000);

        // Its re-alignment step fails: D stays, the fleet D stays, announces back off.
        c.service_date_offset();
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 250_000));
        assert!(c.date_sync.step_failed_at.is_some());
        let now_ptp = wall_now_ns() - (d - 250_000);
        assert_eq!(
            c.date_sync
                .authority
                .as_ref()
                .unwrap()
                .in_effect_ns(now_ptp),
            d,
            "a failed step never moves the fleet D"
        );
        // No retry storm inside the backoff (a second step_clock call would panic the mock).
        c.service_date_offset();

        // After the backoff the re-alignment is retried and lands.
        c.date_sync.step_failed_at =
            Some(Instant::now() - STEP_FAILURE_BACKOFF - Duration::from_secs(1));
        c.service_date_offset();
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
        assert!(c.date_sync.step_failed_at.is_none());
    }

    #[test]
    fn a_master_without_ptp_still_keeps_the_fleet_line_on_utc_88() {
        // ONLY the master lost PTP; its own wall runs the local NTP path (here it is also 250 µs
        // off the fleet line already). Its UTC reading still disciplines the FLEET line: the
        // authority is fed `reading + (anchor − fleet)` and announces for the fleet, while the
        // master neither schedules that step for itself nor stops its own local path.
        let mut ntp = MockNtpSource::new();
        ntp.expect_get_offset()
            .returning(|| Ok(one_offset(60_000, 1)));
        let mut clock = MockSystemClock::new();
        // Its own wall: the legacy server step path steps the full 60 ms on the 2nd reading.
        clock
            .expect_step_clock()
            .times(1)
            .withf(|dur, sign| *dur == Duration::from_millis(60) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, ntp, true);
        c.note_local_date_step(-250_000);
        c.ptp_offline = true;
        c.service_date_offset();
        assert!(!c.date_sync.core.engaged(), "no PTP, no phase lock");
        let seq = c.date_sync.authority.as_ref().unwrap().seq();
        for _ in 0..2 {
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }
        let a = c.date_sync.authority.as_ref().unwrap();
        assert_eq!(a.seq(), seq + 1, "the fleet line's error was announced");
        assert_eq!(
            a.announce().date_offset_ns,
            d + 60_000_000 - 250_000,
            "fleet D + (reading + anchor − fleet)"
        );
        assert!(
            c.date_sync.follower.pending().is_none(),
            "an off-line master does not schedule the fleet step for its own wall"
        );
        assert_eq!(
            c.date_sync.core.anchor_ns(),
            Some(d - 250_000 + 60_000_000),
            "its own wall took the local NTP step"
        );
    }

    #[test]
    fn the_master_re_aligns_only_on_a_fresh_window_and_removes_the_measured_error_88() {
        let mut clock = MockSystemClock::new();
        // fleet − anchor − e = 250 µs − 30 µs.
        clock
            .expect_step_clock()
            .times(1)
            .withf(|dur, sign| *dur == Duration::from_micros(220) && *sign == 1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), true);
        c.note_local_date_step(-250_000);
        // PTP offline: no re-alignment, and the last window is stale from now on.
        c.ptp_offline = true;
        c.service_date_offset();
        c.ptp_offline = false;
        c.service_date_offset(); // back online, but no window since: still nothing
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 250_000));
        // A window after PTP returned measures the free-run error: +30 µs.
        c.is_locked = true;
        c.date_sync.pending_median_ns = Some(d - 250_000 + 30_000);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + 1_000_000_000;
        c.apply_self_tuning_servo(0.0);
        c.service_date_offset();
        assert_eq!(
            c.date_sync.core.anchor_ns(),
            Some(d),
            "on the fleet line, and the error it measured is gone"
        );
    }

    #[test]
    fn a_grandmaster_change_while_the_master_is_off_the_line_shifts_the_fleet_d_117() {
        // The master is 5 ms off the fleet line when the grandmaster changes: the fleet D must
        // follow the BASE shift only — never absorb the master's own 5 ms.
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), true);
        c.note_local_date_step(-5_000_000);
        let shift: i64 = 5 * 86_400 * 1_000_000_000;
        c.current_gm_uuid = Some([0x00, 0x1d, 0xc1, 0x44, 0x55, 0x66]);
        c.date_sync.core.request_rebase();
        c.date_sync.pending_median_ns = Some(d - 5_000_000 + shift);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS - shift;
        c.apply_self_tuning_servo(0.0);
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 5_000_000 + shift));
        let now_ptp_new = wall_now_ns() - (d - 5_000_000 + shift);
        assert_eq!(
            c.date_sync
                .authority
                .as_ref()
                .unwrap()
                .in_effect_ns(now_ptp_new),
            d + shift,
            "the fleet line moved by the base shift, not onto the master's own anchor"
        );
    }
}
