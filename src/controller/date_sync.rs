//! dantesync#117 / #88 — the controller's PTP phase lock and fleet date-offset glue.
//!
//! The laws live in pure modules — `crate::ptp_phase_lock` (the PI on `e = (t2 − t1) − D`) and
//! `crate::date_offset` (the authority, the step scheduler, the time-base check) — proven there and
//! end-to-end by `tests/two_clock_bench.rs`. This file only wires them to the controller: which
//! word drives the clock, the NTP master's authority (its NTP reading becomes an announce, never a
//! step), a follower's poll / join / schedule, the coordinated step itself, the anchor lifecycle,
//! and what `/status` publishes. A child module of `controller` so it reaches the controller's
//! private state; all of its state is the one [`DateSync`] field.
//!
//! dantesync#119 — a backward fleet correction is a coordinated SLEW (`crate::date_offset`): `D`
//! follows the slew's schedule of PTP time. Here it becomes (1) an extra rate term of the ONE
//! frequency word — composed with the phase lock's word in `apply_self_tuning_servo` and
//! re-applied from the loop at the slew's start and end instants ([`PtpController::apply_slew_edge`]),
//! and (2) a de-slew of every PTP sample by the scheduled displacement before either servo reads it
//! ([`DateSync::deslew_sample`]). (2) is the decoupling: the phase lock and the rate servo see the
//! clock as if no slew ran, so neither reads the deliberate rate as grandmaster disagreement, and
//! the only residual they see is the rate term's switching delay (one loop iteration).

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
    /// dantesync#119 — the rate of a backward correction's slew (the master's authority uses it).
    pub(super) slew_ppm: u32,
    /// dantesync#119 — the slew rate term currently inside the applied frequency word (ppm).
    pub(super) applied_slew_ppm: f64,
    /// dantesync#119 — slews already folded into the anchor, as the RATE servo's measurement still
    /// needs them removed (mod 1 s): its phase is continuous across a fold only this way.
    pub(super) rate_folded_ns: i64,
    /// dantesync#119 — when a failed slew-edge write, and a saturated word, were last warned
    /// about (each throttled on its own: the loop runs every 1 ms / 50 µs).
    pub(super) slew_write_warned_at: Option<Instant>,
    pub(super) slew_saturation_warned_at: Option<Instant>,
    /// dantesync#119 — the slew whose START was logged (one line per slew, however often its
    /// word is re-applied).
    pub(super) slew_start_logged: Option<crate::date_offset::DateSlew>,
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
            slew_ppm: config.date_offset.slew_ppm(),
            applied_slew_ppm: 0.0,
            rate_folded_ns: 0,
            slew_write_warned_at: None,
            slew_saturation_warned_at: None,
            slew_start_logged: None,
        }
    }

    /// One accepted PTP sample: the RAW offset between the two time bases (not the mod-1 s
    /// display phase, not calibration-corrected) — what the phase lock holds equal to `D`.
    pub(super) fn push_raw_sample(&mut self, t1_ns: i64, t2_ns: i64) {
        if self.enabled {
            self.window.push(t2_ns.wrapping_sub(t1_ns));
        }
    }

    /// The sample window closed: hand its raw median and grandmaster time to the servo.
    pub(super) fn close_raw_window(&mut self, master_time_ns: i64) {
        self.pending_median_ns = if self.window.is_empty() {
            None
        } else {
            let mut raw = self.window.clone();
            raw.sort_unstable();
            Some(raw[raw.len() / 2])
        };
        self.pending_t1_ns = master_time_ns;
        self.window.clear();
    }

    /// The PTP sender may carry another time base (another grandmaster, or the same one after a
    /// reboot): drop the raw window and re-anchor `D` from the next one, so the wall stays
    /// continuous (never a wall step).
    pub(super) fn on_time_base_change(&mut self) {
        self.window.clear();
        self.core.request_rebase();
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

        let now_wall = wall_now_ns();
        // #119: D IN EFFECT — the anchor plus a held slew's displacement.
        let anchor = if ds.enabled && !ds.core.rebase_pending() {
            ds.d_in_effect(now_wall)
        } else {
            None
        };
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
        // #119: a slew is published by its own fields (never as a pending step), and this box's
        // own progress through the slew it follows.
        let published_slew = anchor.and(published.and_then(|p| p.as_slew()));
        status.date_slew_from_ns = published_slew.map(|s| s.from_ns);
        status.date_slew_to_ns = published_slew.map(|s| s.to_ns);
        status.date_slew_ppm = published_slew.map(|s| s.ppm);
        let base = ds.core.anchor_ns();
        status.date_slew_active =
            ds.enabled && base.is_some_and(|b| ds.follower.slew_rate_ppm(b, now_wall) != 0.0);
        status.date_slew_remaining_ms = if ds.enabled {
            base.and_then(|b| ds.follower.slew_remaining_ns(b, now_wall))
                .map(|n| n as f64 / 1e6)
        } else {
            None
        };
        // The master publishes exactly its authority's announce: D in effect on its own wall
        // (the anchor) plus the difference to the announced D. While a step is pending that is
        // the step; once its instant has passed but before this loop applies it, or while the
        // master is off the fleet line (its own PTP outage, a failed step), it is the correction
        // back to the fleet D — so a follower always reads the FLEET D, never the master's own.
        match (ds.authority.as_ref(), anchor) {
            (Some(_), Some(_)) if published_slew.is_some() => {
                status.date_step_pending_ns = None;
                status.date_step_due_in_ms = None;
            }
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
            // #119: likewise its own slew stops where it is (D stays continuous).
            if let Some(anchor) = self.date_sync.core.anchor_ns() {
                self.date_sync.follower.freeze_slew(anchor, wall_now_ns());
            }
        }
        self.date_sync.last_step =
            Some((delta_ns, (wall_now_ns() / 1_000_000_000) as u64, "local"));
        // The published D in effect (and the replier's PTP now derived from it) moved: publish.
        self.update_shared_status();
    }

    /// #117 — the grandmaster UUID changed: its uptime is a different time base. A whole fresh
    /// window in the new base for both servos, and `D` re-anchored from it.
    pub(super) fn on_grandmaster_uuid_change(&mut self) {
        if self.date_sync.enabled {
            self.sample_window.clear();
            self.date_sync.on_time_base_change();
        }
    }

    /// #117 — the throttled phase-lock line beside the `[PTP]` drift line.
    pub(super) fn log_phase_lock_word(&self, applied_word: f64) {
        if self.date_sync.core.engaged() {
            info!(
                "[PHASE-LOCK] e={:+.1}us word={:+.3}ppm D-seq={}",
                self.date_sync.core.last_error_ns().unwrap_or(0) as f64 / 1_000.0,
                applied_word,
                self.date_sync
                    .follower
                    .adopted_seq()
                    .map(|q| q.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
        }
    }

    /// #117 — this box just lost PTP. Every PHASE measurement taken before the outage is dropped,
    /// exactly as on a grandmaster change: the first window after PTP returns must hold only
    /// post-outage samples, or a pre-outage median hides the free-run error (e = 0) and it is
    /// slewed for minutes instead of re-aligned. The frequency is held at the learned integrator.
    /// The spike filter's RATE history is kept: the oscillator's rate is a hardware property that
    /// survives an outage (an outage, unlike a step, puts no transient into the rate).
    pub(super) fn on_ptp_offline_edge(&mut self) {
        if !self.date_sync.enabled {
            return;
        }
        self.sample_window.clear();
        self.date_sync.window.clear();
        self.date_sync.pending_median_ns = None;
        self.date_sync.last_t1_ns = None;
        self.date_sync.fresh_window = false;
        self.pending_syncs.clear();
        self.prev_t1_ns = 0;
        self.prev_t2_ns = 0;
        self.last_offset_us = None;
        self.last_offset_time = None;
        if self.date_sync.core.engaged() {
            self.date_sync.core.disengage();
            let word = self.date_sync.core.integrator_ppm();
            self.drift_baseline_ppm = word;
            // Hold the LEARNED frequency (not the last word, which carries the last P term), so
            // the free-run through the outage is as small as the oscillator allows.
            self.last_adj_ppm = word;
            self.applied_freq_ppm = word;
            // #119: a running slew keeps its rate term through the outage (open loop, with the
            // rest of the fleet).
            let total = self.compose_slew_word(word);
            if let Err(e) = self.clock.adjust_frequency(1.0 + total / 1_000_000.0) {
                warn!("[PHASE-LOCK] holding {:+.3}ppm failed: {}", word, e);
            }
            info!(
                "[PHASE-LOCK] disengaged (PTP offline) — holding the learned {:+.3}ppm",
                word
            );
        }
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
            self.date_sync.d_in_effect(wall_now_ns()),
            self.date_sync.authority.as_ref(),
        ) else {
            return;
        };
        let now_ptp = wall_now_ns().wrapping_sub(anchor);
        // #119: while the fleet slews, the master only makes sure it slews WITH it (a slew or an
        // extension its own scheduler missed); any other re-alignment waits for the slew's end.
        if a.slew_in_progress(now_ptp).is_some() {
            self.catch_up_fleet_slew();
            return;
        }
        if a.pending_step_ns(now_ptp).is_some()
            || self.date_sync.follower.pending().is_some()
            || self.date_sync.follower.held_slew().is_some()
        {
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
        let Some(base) = self.date_sync.core.anchor_ns() else {
            return false;
        };
        if self.ntp_server_mode {
            self.ensure_date_authority();
            let now_wall = wall_now_ns();
            // #119: the master's D IN EFFECT (its anchor plus a held slew's displacement).
            let anchor = self.date_sync.follower.in_effect_ns(base, now_wall);
            let now_ptp = now_wall.wrapping_sub(anchor);
            let Some(fleet) = self
                .date_sync
                .authority
                .as_ref()
                .map(|a| a.in_effect_ns(now_ptp))
            else {
                return false;
            };
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
                let off_line = if on_line {
                    ""
                } else {
                    " (this master is off the fleet line: it re-aligns afterwards)"
                };
                match ann.as_slew() {
                    // #119: a backward correction — a coordinated slew, never a backward step.
                    Some(sl) => info!(
                        "[DATE] AUTHORITY: the fleet line is {:+}us off UTC (> {}us) — announcing a \
                         fleet date SLEW of {:+}us at {} ppm ({} s) from PTP {} (in {} ms), seq {}{}",
                        fleet_err / 1_000,
                        self.date_sync.step_bound_ns / 1_000,
                        sl.to_ns.wrapping_sub(fleet) / 1_000,
                        sl.ppm,
                        sl.duration_ns() / 1_000_000_000,
                        sl.start_ptp_ns,
                        sl.start_ptp_ns.wrapping_sub(now_ptp) / 1_000_000,
                        ann.seq,
                        off_line
                    ),
                    None => info!(
                        "[DATE] AUTHORITY: the fleet line is {:+}us off UTC (> {}us) — announcing a \
                         fleet date step of {:+}us at PTP {} (in {} ms), seq {}{}",
                        fleet_err / 1_000,
                        self.date_sync.step_bound_ns / 1_000,
                        ann.date_offset_ns.wrapping_sub(fleet) / 1_000,
                        ann.effective_ptp_ns,
                        ann.effective_ptp_ns.wrapping_sub(now_ptp) / 1_000_000,
                        ann.seq,
                        off_line
                    ),
                }
                if on_line {
                    let act = self.date_sync.follower.on_announce(ann, base, now_wall);
                    debug!("[DATE] master's own scheduler: {:?}", act);
                    // #119: an extension heard on the line may land within a ns of D.
                    if let FollowAction::Absorb { new_anchor_ns } = act {
                        self.date_sync.core.set_anchor(new_anchor_ns);
                    }
                }
            }
            // Publish NOW: the 31900 time server reads this snapshot, and an announce heard only
            // at the next 10 s status tick would arrive after its 5 s lead (a late step).
            self.update_shared_status();
            if self.ptp_offline {
                // Its OWN wall keeps the local NTP step path while it has no PTP.
                return false;
            }
            // The NTP step path is bypassed: nothing pending, nothing starved.
            self.ntp_pending_step = None;
            self.ntp_server_checks_since_step = 0;
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
        let now_wall = wall_now_ns();
        // #119: the D IN EFFECT (the anchor plus a held slew's displacement).
        let Some(anchor) = self.date_sync.d_in_effect(now_wall) else {
            return;
        };
        let base = self.date_sync.core.anchor_ns().unwrap_or(anchor);
        let authority = DateAuthority::new(
            anchor,
            now_wall.wrapping_sub(anchor),
            self.date_sync.step_bound_ns,
            self.date_sync.step_lead_ns,
        )
        .with_slew_ppm(self.date_sync.slew_ppm);
        let act = self
            .date_sync
            .follower
            .on_announce(authority.announce(), base, now_wall);
        debug!("[DATE] master aligned with its own authority: {:?}", act);
        info!(
            "[DATE] this NTP master is the fleet DATE-OFFSET AUTHORITY: D={}ns, step bound {} ms, \
             announce lead {} s — clients step forward together at the announced PTP instant, \
             and slew backward corrections at {} ppm (never a backward step)",
            anchor,
            self.date_sync.step_bound_ns / 1_000_000,
            self.date_sync.step_lead_ns / 1_000_000_000,
            authority.slew_ppm()
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
                // #119: "now" in the OLD base comes from the D IN EFFECT (the old anchor plus a
                // held slew's displacement, which a rebase does not change) — then the held slew
                // moves into the new base with the anchor, so it runs at the same wall instants.
                let wall = wall_now_ns();
                let disp = self.date_sync.follower.displacement_at_wall(old_ns, wall);
                self.date_sync
                    .follower
                    .rebase_slew(new_ns.wrapping_sub(old_ns));
                if let Some(a) = self.date_sync.authority.as_mut() {
                    // "now" in the OLD base: the wall did not move, the base did. The FLEET line is
                    // shifted by the observed base shift — not set to this master's own anchor,
                    // which may be off the fleet line (its own PTP outage, a failed step): folding
                    // that offset in would reach every follower as a step.
                    let now_ptp_old = wall.wrapping_sub(old_ns).wrapping_sub(disp);
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
        if self.ptp_offline && self.date_sync.core.engaged() {
            // No PTP, no phase lock (normally done on the offline edge already).
            self.on_ptp_offline_edge();
        }
        // #119: a slew whose amount is paid is folded into the anchor, then the rate term follows
        // the slew's schedule at this very instant (its start and end land within one loop
        // iteration on every box, like a coordinated step).
        let now_wall = wall_now_ns();
        if let Some(fold) = self.date_sync.fold_completed_slew(now_wall) {
            info!(
                "[DATE] slew DONE: D moved {:+}us, no wall step (seq {})",
                fold / 1_000,
                self.date_sync
                    .follower
                    .adopted_seq()
                    .map(|q| q.to_string())
                    .unwrap_or_else(|| "-".to_string())
            );
            self.update_shared_status();
        }
        self.apply_slew_edge(now_wall);
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
        // #119: this box's PTP view comes from its D IN EFFECT at the reply's arrival.
        let d_at_reply = self
            .date_sync
            .follower
            .in_effect_ns(anchor, reply.received_wall_ns);
        if Some(ext.gm_uuid) != self.date_sync.anchor_gm
            || !same_time_base(ext.now_ptp_ns, reply.received_wall_ns, d_at_reply)
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
            FollowAction::SlewScheduled {
                amount_ns,
                start_wall_ns,
                ppm,
            } => info!(
                "[DATE] coordinated date SLEW {:+}us at {} ppm scheduled (seq {}) in {} ms — no \
                 backward step",
                amount_ns / 1_000,
                ppm,
                ext.announce.seq,
                start_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
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
            // master publishes the fleet D regardless (still feeding the fleet line's UTC error)
            // and steps its own wall to it after the backoff (`realign_master_to_fleet`).
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

mod slew;

#[cfg(test)]
mod tests;
