//! dantesync#119 — the controller wiring of the coordinated date slew: no backward step, the rate
//! term inside the one frequency word and switched from the loop, every PTP sample de-slewed, the
//! fold, the master's catch-up and what `/status` publishes. Reuses the `date_sync` test helpers.

use super::super::tests::{
    authority_reply, one_offset, phase_lock_config, with_authority, PL_GM, PL_PTP_NOW_NS,
};
use super::super::*;
use crate::clock::MockSystemClock;
use crate::date_offset::{DateSlew, SlewSpec};
use crate::traits::{MockNtpSource, MockPtpNetwork};

// ========================================================================
// dantesync#119 — A BACKWARD CORRECTION IS A COORDINATED SLEW
// ========================================================================
//
// The slew's law (the direction decision, the schedule, the follower's hold / join / fold) is
// proven in `crate::date_offset` and end-to-end in `tests/two_clock_bench.rs`; these pin the
// controller wiring: no backward step anywhere, the rate term inside the ONE frequency word and
// switched from the loop at the instants, every PTP sample de-slewed, and what /status publishes.

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

    // Every PTP sample is de-slewed: a sample on the slewed line (zero path delay) — built from
    // the slew's own SCHEDULE at chosen PTP instants, not from the controller's solve — reads
    // exactly the anchor, i.e. the phase lock sees no slew at all.
    let held = c.date_sync.follower.held_slew().expect("held");
    let p_now = wall_now_ns() - c.date_sync.d_in_effect(wall_now_ns()).unwrap();
    for dt in [0, 1_000_000_000, 10_000_000_000, 400_000_000_000] {
        let p = p_now + dt + 123_457;
        let wall = p + anchor + held.displacement_at_ptp(p);
        let (t2_lock, _) = c.date_sync.deslew_sample(wall);
        assert!(
            ((t2_lock - p) - anchor).abs() <= 1,
            "the phase lock sees the slew at +{dt} ns: {} ns",
            (t2_lock - p) - anchor
        );
    }

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
    // A 20 µs slew at 100 ppm (200 ms), starting 100 ms from now: wide enough that a scheduler
    // stall of the test thread cannot skip the whole slew.
    let slew = DateSlew {
        from_ns: d,
        to_ns: d - 20_000,
        start_ptp_ns: now_ptp + 100_000_000,
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
        Some(d - 20_000),
        "the paid amount folded into the anchor"
    );
    assert_eq!(c.date_sync.slew_rate_ppm(wall_now_ns()), 0.0);
}

#[test]
fn the_master_catches_up_with_a_fleet_slew_its_own_scheduler_missed_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(60_000, -1)));
    // No step_clock expectation: the catch-up never steps.
    let (mut c, d, _words) = capturing_anchored_controller(true, ntp);
    for _ in 0..2 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    let fleet_slew = c.date_sync.follower.held_slew().expect("scheduled").slew;
    // Its own scheduler missed the announce (as if the master was in its step backoff then).
    c.date_sync.follower = DateFollower::new();
    let wall = wall_now_ns();
    let aligned = DateAnnounce {
        date_offset_ns: d,
        effective_ptp_ns: 0,
        seq: 1,
        slew: None,
    };
    c.date_sync.follower.on_announce(aligned, d, wall);
    assert!(c.date_sync.follower.held_slew().is_none());
    c.service_date_offset();
    assert_eq!(
        c.date_sync.follower.held_slew().map(|h| h.slew),
        Some(fleet_slew),
        "the master slews with the fleet it announced to"
    );
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d), "no step, no jump");
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
