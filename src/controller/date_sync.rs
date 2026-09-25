//! dantesync#117 / #88 — the controller's fleet date-offset glue.
//!
//! The laws live in pure modules — `crate::ptp_phase_lock` (the PI on `e = (t2 − t1) − D`) and
//! `crate::date_offset` (the authority and the step scheduler) — proven there and end-to-end by
//! `tests/two_clock_bench.rs`. This file only wires them to the controller: the NTP master's
//! authority (its NTP reading becomes an announce, never a step), a follower's poll / join /
//! schedule, the coordinated step itself, and the anchor lifecycle. A child module of
//! `controller` so it reaches the controller's private state without growing `controller.rs`.

use super::*;
use crate::date_offset::{FollowAction, StepKind};
use crate::ptp_phase_lock::AnchorEvent;

/// dantesync#88 — a reply from the date-offset authority older than this is not acted on (the
/// poller asks once per second; a stale reply means the master went quiet).
const AUTHORITY_REPLY_MAX_AGE: Duration = Duration::from_secs(5);

fn step_kind_label(kind: StepKind) -> &'static str {
    match kind {
        StepKind::Join => "join",
        StepKind::Coordinated => "coordinated",
        StepKind::Late => "late",
    }
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// #117 — the LOCAL date path stepped the wall by `delta_ns` (the NTP step path, used while no
    /// authority is heard or PTP is offline). `D` moves with the wall so the phase lock sees no
    /// disturbance; on the master the fleet offset is rebased (no coordinated step: this one has
    /// already happened) so followers re-align at their next poll.
    pub(super) fn note_local_date_step(&mut self, delta_ns: i64) {
        self.phase_lock.note_step(delta_ns);
        if let Some(new_anchor) = self.phase_lock.anchor_ns() {
            // The PTP time did not move: now in the (unchanged) base = wall − the new D.
            let now_ptp = wall_now_ns().wrapping_sub(new_anchor);
            if let Some(a) = self.date_authority.as_mut() {
                let ann = a.rebase(new_anchor, now_ptp);
                info!(
                    "[DATE] authority rebased after a local step of {:+}us (seq {})",
                    delta_ns / 1_000,
                    ann.seq
                );
            }
        }
        self.last_date_step = Some((delta_ns, (wall_now_ns() / 1_000_000_000) as u64, "local"));
    }

    /// #117 / #88 — the NTP reading under the phase lock. Returns true when it was fully handled
    /// here (the caller must NOT run the NTP step path):
    ///
    /// - the NTP master with an anchor and PTP online feeds `UTC − wall` to the date-offset
    ///   authority, which may announce a coordinated step (it never steps here);
    /// - a follower aligned with the authority only reports the reading (its date moves only at
    ///   announced instants).
    ///
    /// Everything else (legacy discipline, not anchored yet, PTP offline, no authority heard)
    /// returns false and keeps the existing NTP step path — the local date fallback.
    pub(super) fn ntp_under_date_authority(&mut self, offset_us: i64) -> bool {
        if !self.phase_lock_enabled || self.ptp_offline {
            return false;
        }
        let Some(anchor) = self.phase_lock.anchor_ns() else {
            return false;
        };
        if self.ntp_server_mode {
            self.ensure_date_authority();
            let now_wall = wall_now_ns();
            let now_ptp = now_wall.wrapping_sub(anchor);
            let err_ns = offset_us.saturating_mul(1_000);
            self.master_utc_error_ns = Some(err_ns);
            // Log-surface contract: every NTP cycle keeps the exact `[NTP] offset:{:+}us` prefix
            // the camera-box freshness gates parse.
            info!(
                "[NTP] offset:{:+}us (date authority, step bound {}us)",
                offset_us,
                self.date_step_bound_ns / 1_000
            );
            let announced = self
                .date_authority
                .as_mut()
                .and_then(|a| a.on_utc_error(err_ns, now_ptp));
            if let Some(ann) = announced {
                let step_ns = ann.date_offset_ns.wrapping_sub(anchor);
                info!(
                    "[DATE] AUTHORITY: UTC − wall = {:+}us exceeds {}us — announcing a fleet date \
                     step of {:+}us at PTP {} (in {} ms), seq {}",
                    offset_us,
                    self.date_step_bound_ns / 1_000,
                    step_ns / 1_000,
                    ann.effective_ptp_ns,
                    ann.effective_ptp_ns.wrapping_sub(now_ptp) / 1_000_000,
                    ann.seq
                );
                let act = self.date_follower.on_announce(ann, anchor, now_wall);
                debug!("[DATE] master's own scheduler: {:?}", act);
            }
            // The NTP step path is bypassed: nothing pending, nothing starved.
            self.ntp_pending_step = None;
            self.ntp_server_checks_since_step = 0;
            self.update_shared_status();
            return true;
        }
        if self.date_follower.adopted() {
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
        if !self.phase_lock_enabled || !self.ntp_server_mode || self.date_authority.is_some() {
            return;
        }
        let Some(anchor) = self.phase_lock.anchor_ns() else {
            return;
        };
        let now_wall = wall_now_ns();
        let authority = DateAuthority::new(
            anchor,
            now_wall.wrapping_sub(anchor),
            self.date_step_bound_ns,
            self.date_step_lead_ns,
        );
        let act = self
            .date_follower
            .on_announce(authority.announce(), anchor, now_wall);
        debug!("[DATE] master aligned with its own authority: {:?}", act);
        info!(
            "[DATE] this NTP master is the fleet DATE-OFFSET AUTHORITY: D={}ns, step bound {} ms, \
             announce lead {} s — clients step together at the announced PTP instant",
            anchor,
            self.date_step_bound_ns / 1_000_000,
            self.date_step_lead_ns / 1_000_000_000
        );
        self.date_authority = Some(authority);
    }

    /// #117 — react to the phase lock's anchor lifecycle.
    pub(super) fn handle_phase_anchor_event(&mut self, event: AnchorEvent) {
        match event {
            AnchorEvent::None => {}
            AnchorEvent::Anchored { anchor_ns } => {
                self.anchor_gm = self.current_gm_uuid;
                info!(
                    "[PHASE-LOCK] anchored: wall = PTP time + D, D={}ns (grandmaster {})",
                    anchor_ns,
                    self.current_gm_uuid
                        .as_ref()
                        .map(format_mac)
                        .unwrap_or_else(|| "?".to_string())
                );
                self.ensure_date_authority();
            }
            AnchorEvent::Rebased { old_ns, new_ns } => {
                self.anchor_gm = self.current_gm_uuid;
                info!(
                    "[PHASE-LOCK] re-anchored on the grandmaster's time base, wall continuous \
                     (no step): D {} -> {} ns (grandmaster {})",
                    old_ns,
                    new_ns,
                    self.current_gm_uuid
                        .as_ref()
                        .map(format_mac)
                        .unwrap_or_else(|| "?".to_string())
                );
                if let Some(a) = self.date_authority.as_mut() {
                    // "now" in the OLD base: the wall did not move, the base did.
                    let ann = a.rebase(new_ns, wall_now_ns().wrapping_sub(old_ns));
                    info!(
                        "[DATE] authority rebased onto the new time base (seq {})",
                        ann.seq
                    );
                }
            }
        }
    }

    /// #88 — every loop iteration: apply a coordinated step whose instant has come, and (a
    /// follower) act once on each new announce from the master.
    pub(super) fn service_date_offset(&mut self) {
        if !self.phase_lock_enabled || self.phase_lock.anchor_ns().is_none() {
            return;
        }
        if let Some(due) = self.date_follower.due(wall_now_ns()) {
            self.apply_date_step(due.delta_ns, StepKind::Coordinated, due.seq);
        }
        if self.ntp_server_mode {
            self.ensure_date_authority();
            return;
        }
        let Some(reply) = self.date_authority_source.latest() else {
            return;
        };
        if reply.serial == self.last_authority_serial {
            return;
        }
        self.last_authority_serial = reply.serial;
        if reply.received.elapsed() > AUTHORITY_REPLY_MAX_AGE {
            return;
        }
        let Some(ext) = reply.ext.filter(|e| e.authority) else {
            return;
        };
        // D belongs to the master's PTP time base: adopt it only on the same grandmaster, and
        // only while this box's own anchor is in that base (not mid re-anchor, PTP online).
        if self.ptp_offline || reply.gm_uuid.is_none() || reply.gm_uuid != self.anchor_gm {
            debug!(
                "[DATE] authority announce seq {} not applicable here (master GM {:?}, ours {:?}, \
                 ptp_offline {})",
                ext.announce.seq, reply.gm_uuid, self.anchor_gm, self.ptp_offline
            );
            return;
        }
        let Some(anchor) = self.phase_lock.anchor_ns() else {
            return;
        };
        let now_wall = wall_now_ns();
        let first = !self.date_follower.adopted();
        self.last_date_announce = Some(ext.announce);
        match self
            .date_follower
            .on_announce(ext.announce, anchor, now_wall)
        {
            FollowAction::None => {}
            FollowAction::Absorb { new_anchor_ns } => {
                self.phase_lock.set_anchor(new_anchor_ns);
                if first {
                    info!(
                        "[DATE] aligned with the fleet date offset (seq {}): D adopted, \
                         {:+}ns inside the absorb tolerance — no step",
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
            // D is NOT moved: the next authority poll sees the difference and re-joins.
            warn!(
                "[DATE] {} date step {:+}us (seq {}) FAILED: {} — re-aligning at the next poll",
                label,
                delta_ns / 1_000,
                seq,
                e
            );
            return;
        }
        self.phase_lock.note_step(delta_ns);
        self.reset_ptp_measurement_after_step();
        self.ntp_offset_samples.clear();
        self.ntp_pending_step = None;
        self.last_date_step = Some((delta_ns, (wall_now_ns() / 1_000_000_000) as u64, label));
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
        c.pending_phase_median_ns = Some(d);
        c.pending_phase_t1_ns = PL_PTP_NOW_NS;
        c.apply_self_tuning_servo(0.0);
        assert_eq!(
            c.phase_lock.anchor_ns(),
            Some(d),
            "the first lock anchors D"
        );
        assert!(c.phase_lock.engaged());
        (c, d)
    }

    fn authority_reply(
        serial: u64,
        gm: [u8; 6],
        date_offset_ns: i64,
        effective_ptp_ns: i64,
        seq: u32,
    ) -> crate::time_server::AuthorityReply {
        crate::time_server::AuthorityReply {
            serial,
            gm_uuid: Some(gm),
            is_locked: true,
            ext: Some(crate::date_offset::DateExtension {
                version: crate::date_offset::EXT_VERSION,
                authority: true,
                announce: DateAnnounce {
                    date_offset_ns,
                    effective_ptp_ns,
                    seq,
                },
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
        c.pending_phase_median_ns = Some(d);
        c.pending_phase_t1_ns = PL_PTP_NOW_NS;
        c.apply_self_tuning_servo(0.0);
        // Bumpless: the first word is the rate servo's (12 ppm, rate 0).
        // A +500 µs phase error half a second later (grandmaster time) pulls the word DOWN.
        c.pending_phase_median_ns = Some(d + 500_000);
        c.pending_phase_t1_ns = PL_PTP_NOW_NS + 500_000_000;
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
            c.phase_lock.anchor_ns(),
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
        // The master announces +60 ms, 150 ms ahead of now (in PTP time).
        let now_ptp = wall_now_ns() - d;
        *slot.lock().unwrap() = Some(authority_reply(
            2,
            PL_GM,
            d + 60_000_000,
            now_ptp + 150_000_000,
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
        std::thread::sleep(Duration::from_millis(200));
        c.service_date_offset(); // at/after the instant: the step lands
        assert_eq!(c.phase_lock.anchor_ns(), Some(d + 60_000_000));
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
        assert_eq!(c.phase_lock.anchor_ns(), Some(d));
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
            c.date_authority.is_some(),
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
        let seq_before = c.date_authority.as_ref().unwrap().seq();
        let new_gm = [0x00, 0x1d, 0xc1, 0x44, 0x55, 0x66];
        c.current_gm_uuid = Some(new_gm);
        c.phase_lock.request_rebase();
        // The new grandmaster's uptime is 5 days behind: t2 − t1 grows by 5 days.
        let five_days: i64 = 5 * 86_400 * 1_000_000_000;
        c.pending_phase_median_ns = Some(d + five_days);
        c.pending_phase_t1_ns = PL_PTP_NOW_NS - five_days;
        c.apply_self_tuning_servo(0.0);
        assert_eq!(c.phase_lock.anchor_ns(), Some(d + five_days));
        assert_eq!(c.anchor_gm, Some(new_gm));
        assert_eq!(c.date_authority.as_ref().unwrap().seq(), seq_before + 1);
        assert_eq!(c.phase_lock.last_error_ns(), Some(0));
    }
}
