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
                slew: None,
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

#[test]
fn an_off_line_masters_announce_is_published_at_once_not_at_the_next_tick_88() {
    // The 31900 time server reads the status snapshot: an announce the off-line master makes
    // must be in it immediately, or followers hear it after its 5 s lead (a late step).
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(60_000, 1)));
    let mut clock = MockSystemClock::new();
    clock.expect_step_clock().returning(|_, _| Ok(()));
    let (mut c, _d) = anchored_controller(clock, ntp, true);
    c.ptp_offline = true;
    for _ in 0..2 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    let ann = c.date_sync.authority.as_ref().unwrap().announce();
    assert_eq!(ann.seq, 2, "the fleet step was announced");
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(
        st.date_offset_seq,
        Some(2),
        "published without waiting for tick_status"
    );
    assert_eq!(st.date_offset_effective_ptp_ns, Some(ann.effective_ptp_ns));
}

#[test]
fn samples_from_before_a_ptp_outage_never_mix_into_the_first_window_after_it_117() {
    // Driven through the real sample path. The window is 4 samples; 3 are collected, then
    // PTP drops out, the wall free-runs 3 ms, and PTP returns. The first window after the
    // outage must hold only post-outage samples, so the 3 ms re-anchors (Realigned) instead
    // of hiding behind a pre-outage median (e = 0) and being slewed for minutes.
    let mut clock = MockSystemClock::new();
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        phase_lock_config(),
    );
    c.current_gm_uuid = Some(PL_GM);
    c.is_locked = true;
    let d: i64 = 1_790_000_000 * 1_000_000_000 - 5_000 * 1_000_000_000;
    let mut t1: i64 = 5_000 * 1_000_000_000;
    let feed = |c: &mut PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
                t1: i64,
                offset: i64| {
        let t2 = t1 + offset;
        let phase = c.calculate_phase_offset(t1, t2);
        c.process_settled_sync(t1, t2, phase);
    };
    for _ in 0..4 {
        t1 += 125_000_000;
        feed(&mut c, t1, d);
    }
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d),
        "anchored on the first window"
    );
    for _ in 0..3 {
        t1 += 125_000_000;
        feed(&mut c, t1, d);
    }
    // PTP drops out …
    c.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);
    c.check_ptp_status();
    assert!(c.ptp_offline);
    c.service_date_offset();
    // … and comes back 30 s later with the wall 3 ms ahead of the D line.
    c.last_ptp_packet = Instant::now();
    c.check_ptp_status();
    assert!(!c.ptp_offline);
    t1 += 30_000_000_000;
    for _ in 0..4 {
        t1 += 125_000_000;
        feed(&mut c, t1, d + 3_000_000);
    }
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d + 3_000_000),
        "re-aligned on a window of post-outage samples only"
    );
}

#[test]
fn a_ptp_outage_holds_the_learned_integrator_not_the_last_word_117() {
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
    // A +500 µs error: the last word carries a large P term on top of the learned frequency.
    c.date_sync.pending_median_ns = Some(d + 500_000);
    c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + 500_000_000;
    c.apply_self_tuning_servo(0.0);
    let learned = c.date_sync.core.integrator_ppm();
    let last_word = *captured.lock().expect("cap").last().expect("a word");
    assert!(
        (last_word - learned).abs() > 1.0,
        "the last word carries a P term: {last_word} vs the learned {learned}"
    );

    // PTP drops out.
    c.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);
    c.check_ptp_status();
    assert!(c.ptp_offline);
    assert!(!c.date_sync.core.engaged(), "no PTP, no phase lock");
    let held = *captured.lock().expect("cap").last().expect("a word");
    assert!(
        (held - learned).abs() < 1e-9,
        "the clock holds the learned {learned} ppm through the free-run, got {held}"
    );
    assert!((c.applied_freq_ppm - learned).abs() < 1e-9);
    assert!((c.drift_baseline_ppm - learned).abs() < 1e-9);
}

#[test]
fn under_legacy_a_ptp_outage_keeps_every_measurement_and_touches_no_clock_117() {
    // No clock expectation at all: a frequency hold (the phase lock's) would panic the mock.
    let mut config = phase_lock_config();
    config.clock_discipline = CLOCK_DISCIPLINE_LEGACY.to_string();
    let mut c = PtpController::new(
        MockSystemClock::new(),
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    c.sample_window.push(1_000);
    c.sample_window.push(2_000);
    c.prev_t1_ns = 7;
    c.prev_t2_ns = 9;
    c.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);
    c.check_ptp_status();
    assert!(c.ptp_offline);
    assert_eq!(
        c.sample_window,
        vec![1_000, 2_000],
        "legacy keeps its window through an outage (the pre-#117 behaviour)"
    );
    assert_eq!((c.prev_t1_ns, c.prev_t2_ns), (7, 9));
}

// ========================================================================
// dantesync#119 — A BACKWARD CORRECTION IS A COORDINATED SLEW
// ========================================================================
//
// The slew's law (the direction decision, the schedule, the follower's hold / join / fold) is
// proven in `crate::date_offset` and end-to-end in `tests/two_clock_bench.rs`; these pin the
// controller wiring: no backward step anywhere, the rate term inside the ONE frequency word and
// switched from the loop at the instants, every PTP sample de-slewed, and what /status publishes.

use crate::date_offset::{DateSlew, SlewSpec};

/// `anchored_controller`, but every applied frequency word (ppm) is captured.
fn capturing_anchored_controller(
    master: bool,
    ntp: MockNtpSource,
) -> (
    PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
    i64,
    Arc<std::sync::Mutex<Vec<f64>>>,
) {
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
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
    (c, d, captured)
}

/// This box aligned with the authority's `d` (seq 1), then handed `slew` (seq 2) directly.
fn follow_slew(
    c: &mut PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
    d: i64,
    slew: DateSlew,
) -> FollowAction {
    let wall = wall_now_ns();
    let aligned = DateAnnounce {
        date_offset_ns: d,
        effective_ptp_ns: 0,
        seq: 1,
        slew: None,
    };
    assert_eq!(
        c.date_sync.follower.on_announce(aligned, d, wall),
        FollowAction::None
    );
    let act = c
        .date_sync
        .follower
        .on_announce(slew_announce(slew, 2), d, wall);
    if let FollowAction::Absorb { new_anchor_ns } = act {
        c.date_sync.core.set_anchor(new_anchor_ns);
    }
    act
}

fn slew_announce(slew: DateSlew, seq: u32) -> DateAnnounce {
    DateAnnounce {
        date_offset_ns: slew.to_ns,
        effective_ptp_ns: slew.start_ptp_ns,
        seq,
        slew: Some(SlewSpec {
            from_ns: slew.from_ns,
            ppm: slew.ppm,
        }),
    }
}

#[test]
fn the_master_slews_a_negative_utc_error_and_never_steps_it_back_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(60_000, -1)));
    // No step_clock expectation: a backward date step would panic the mock.
    let (mut c, d, _words) = capturing_anchored_controller(true, ntp);
    for _ in 0..2 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(
        st.date_step_pending_ns, None,
        "a backward correction is never a step"
    );
    assert_eq!(st.date_slew_ppm, Some(100));
    assert_eq!(st.date_slew_from_ns, Some(d));
    assert_eq!(st.date_slew_to_ns, Some(d - 60_000_000));
    assert_eq!(st.date_offset_seq, Some(2));
    assert!(
        !st.date_slew_active,
        "scheduled a lead ahead, not running yet"
    );
    assert_eq!(st.date_slew_remaining_ms, Some(60.0));
    assert_eq!(st.date_offset_ns, Some(d), "D unchanged before the start");
    let start = st.date_offset_effective_ptp_ns.expect("start instant");
    let lead = start - (wall_now_ns() - d);
    assert!(
        (4_000_000_000..=5_000_000_000).contains(&lead),
        "the slew starts a lead ahead: {lead} ns"
    );
    // The master's own scheduler holds it: it slews with the fleet.
    assert!(c.date_sync.follower.held_slew().is_some());
}

#[test]
fn a_follower_takes_the_masters_slew_from_the_reply_and_never_steps_119() {
    // No step_clock expectation: any step panics the mock.
    let (mut c, d, _words) = capturing_anchored_controller(false, MockNtpSource::new());
    let slot = with_authority(&mut c);
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 1_000_000_000, 3));
    c.service_date_offset();
    let now_ptp = wall_now_ns() - d;
    let slew = DateSlew {
        from_ns: d,
        to_ns: d - 51_000_000,
        start_ptp_ns: now_ptp + 2_000_000_000,
        ppm: 100,
    };
    let mut reply = authority_reply(2, PL_GM, slew.to_ns, slew.start_ptp_ns, 4);
    if let Some(ext) = reply.ext.as_mut() {
        ext.announce.slew = Some(SlewSpec {
            from_ns: d,
            ppm: 100,
        });
        ext.now_ptp_ns = reply.received_wall_ns - d;
    }
    *slot.lock().unwrap() = Some(reply);
    c.service_date_offset();
    let held = c.date_sync.follower.held_slew().expect("the slew is held");
    assert_eq!(held.slew, slew);
    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_authority, "follower");
    assert_eq!(st.date_step_pending_ns, None);
    assert_eq!(st.date_slew_ppm, Some(100));
    assert_eq!(st.date_slew_remaining_ms, Some(51.0));
    assert_eq!(st.date_offset_seq, Some(4));
    assert_eq!(st.date_steps_late, 0);
}

#[test]
fn a_running_slew_is_one_rate_term_in_the_one_frequency_word_and_is_deslewed_119() {
    let (mut c, d, words) = capturing_anchored_controller(false, MockNtpSource::new());
    let now_ptp = wall_now_ns() - d;
    // Started 0.5 s ago: 50 µs paid; this box hears it now and lands on the fleet's D (an absorb,
    // well inside the 100 µs tolerance however long the test takes to get here).
    let slew = DateSlew {
        from_ns: d,
        to_ns: d - 50_000_000,
        start_ptp_ns: now_ptp - 500_000_000,
        ppm: 100,
    };
    assert!(matches!(
        follow_slew(&mut c, d, slew),
        FollowAction::Absorb { .. }
    ));
    let anchor = c.date_sync.core.anchor_ns().unwrap();
    assert_eq!(c.date_sync.slew_rate_ppm(wall_now_ns()), -100.0);

    // A PTP window with no phase error (the samples are de-slewed): the applied word is the phase
    // lock's own word plus the −100 ppm rate term — composed, one write.
    c.date_sync.pending_median_ns = Some(anchor);
    c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + 500_000_000;
    c.apply_self_tuning_servo(0.0);
    let pi = c.date_sync.core.last_freq_ppm();
    let applied = *words.lock().unwrap().last().unwrap();
    assert!(
        (applied - (pi - 100.0)).abs() < 1e-9,
        "applied {applied} ppm, the law {pi} ppm"
    );
    assert_eq!(
        c.applied_freq_ppm, pi,
        "the servo's own word never carries the slew"
    );

    // Every PTP sample is de-slewed: on the slewed line, the phase lock reads exactly its anchor.
    let wall = wall_now_ns();
    let d_eff = c.date_sync.d_in_effect(wall).unwrap();
    let t1 = wall - d_eff; // a sample on the fleet line (zero path delay)
    let (t2_lock, _) = c.date_sync.deslew_sample(wall);
    assert!(
        ((t2_lock - t1) - anchor).abs() <= 1,
        "the phase lock sees no slew: {} ns",
        (t2_lock - t1) - anchor
    );

    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert!(st.date_slew_active);
    let left = st.date_slew_remaining_ms.expect("running");
    assert!((49.0..50.0).contains(&left), "remaining {left} ms");
}

#[test]
fn the_slew_starts_and_ends_on_the_loop_not_at_the_next_ptp_window_119() {
    // No step_clock expectation: the slew never steps.
    let (mut c, d, words) = capturing_anchored_controller(false, MockNtpSource::new());
    let pi = c.applied_freq_ppm;
    let now_ptp = wall_now_ns() - d;
    // A 2 µs slew at 100 ppm (20 ms), starting 30 ms from now.
    let slew = DateSlew {
        from_ns: d,
        to_ns: d - 2_000,
        start_ptp_ns: now_ptp + 30_000_000,
        ppm: 100,
    };
    assert!(matches!(
        follow_slew(&mut c, d, slew),
        FollowAction::SlewScheduled { .. }
    ));
    words.lock().unwrap().clear();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut folded = false;
    while Instant::now() < deadline && !folded {
        c.service_date_offset(); // the loop — no PTP window runs in this test
        folded = c.date_sync.follower.held_slew().is_none();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(folded, "the slew completed and was folded");
    let seen = words.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "the word was re-applied exactly at the start and at the end: {seen:?}"
    );
    assert!((seen[0] - (pi - 100.0)).abs() < 1e-9, "start: {seen:?}");
    assert!((seen[1] - pi).abs() < 1e-9, "end: {seen:?}");
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d - 2_000),
        "the paid amount folded into the anchor"
    );
    assert_eq!(c.date_sync.slew_rate_ppm(wall_now_ns()), 0.0);
}

#[test]
fn a_fold_keeps_both_servos_measurements_continuous_119() {
    let (mut c, d, _words) = capturing_anchored_controller(false, MockNtpSource::new());
    let now_ptp = wall_now_ns() - d;
    let slew = DateSlew {
        from_ns: d,
        to_ns: d - 1_000,
        start_ptp_ns: now_ptp + 5_000_000,
        ppm: 100,
    };
    follow_slew(&mut c, d, slew);
    std::thread::sleep(Duration::from_millis(40)); // past its 10 ms end
    let wall = wall_now_ns();
    let d_before = c.date_sync.d_in_effect(wall).unwrap();
    let (lock_before, rate_before) = c.date_sync.deslew_sample(wall);
    c.date_sync.window.push(lock_before - 7);
    c.date_sync.pending_median_ns = Some(lock_before - 9);
    let fold = c.date_sync.fold_completed_slew(wall).expect("complete");
    assert_eq!(fold, -1_000);
    assert_eq!(c.date_sync.d_in_effect(wall), Some(d_before), "D unchanged");
    let (lock_after, rate_after) = c.date_sync.deslew_sample(wall);
    // The phase lock: its anchor moved by the fold, so do its samples (e unchanged).
    assert_eq!(lock_after - (d - 1_000), lock_before - d);
    assert_eq!(c.date_sync.window[0], lock_before - 7 + fold);
    assert_eq!(c.date_sync.pending_median_ns, Some(lock_before - 9 + fold));
    // The rate servo's measurement is continuous (mod 1 s: its phase is mod 1 s).
    assert_eq!(
        rate_after.rem_euclid(1_000_000_000),
        rate_before.rem_euclid(1_000_000_000)
    );
}

#[test]
fn legacy_discipline_never_slews_119() {
    let mut config = phase_lock_config();
    config.clock_discipline = CLOCK_DISCIPLINE_LEGACY.to_string();
    let c = PtpController::new(
        MockSystemClock::new(),
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    assert_eq!(c.date_sync.slew_rate_ppm(wall_now_ns()), 0.0);
    assert_eq!(c.date_sync.deslew_sample(123_456), (123_456, 123_456));
}
