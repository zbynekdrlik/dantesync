//! dantesync#119 follow-up — the controller wiring of the date MICRO-corrections: the master's
//! loop drives the authority's micro clock, an increment is applied through the ordinary step /
//! slew paths but labelled `micro` and kept out of the NTP step-storm count, a follower takes a
//! micro announce from the reply, and `/status` publishes `date_micro_*` and the falling-behind
//! alarm. The decision itself is proven in `crate::date_offset::micro` and end-to-end in
//! `tests/two_clock_bench.rs`.

use super::tests::{
    anchored_controller, authority_reply, one_offset, readings_then_tick, with_authority, PL_GM,
};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::MockNtpSource;

#[test]
fn the_master_announces_a_forward_micro_step_from_its_loop_not_at_ntp_time_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(7_000, 1)));
    // No step_clock expectation: nothing is stepped before the announced instant.
    let (mut c, d) = anchored_controller(MockSystemClock::new(), ntp, true);
    for _ in 0..6 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    assert_eq!(
        c.date_sync.authority.as_ref().unwrap().seq(),
        1,
        "a normal reading never announces by itself"
    );
    c.service_date_offset();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_offset_seq, Some(2));
    assert_eq!(
        st.date_step_pending_ns,
        Some(500_000),
        "one 500 µs increment"
    );
    assert!(st.date_offset_micro);
    assert!(st.date_micro_active, "the master's own scheduler holds it");
    assert_eq!(st.date_offset_ns, Some(d), "in effect only at the instant");
    let due = st.date_step_due_in_ms.expect("scheduled");
    assert!(
        (9_000..=10_000).contains(&due),
        "announced two 5 s leads ahead: {due} ms"
    );
    // The correction rate counts it (over the one-minute floor of the covered time).
    assert_eq!(st.date_correction_rate_ms_per_min, Some(0.5));
    assert!(!st.date_correction_falling_behind);
    assert_eq!(st.date_micro_last_us, None, "announced, not applied yet");
}

#[test]
fn an_applied_micro_step_is_labelled_micro_and_never_counts_toward_the_step_storm_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(7_000, 1)));
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sign| *dur == Duration::from_micros(500) && *sign == 1)
        .returning(|_, _| Ok(()));
    let (mut c, d) = anchored_controller(clock, ntp, true);
    readings_then_tick(&mut c);
    let seq = c.date_sync.authority.as_ref().unwrap().seq();
    let storm_before = c.ntp_step_times.len();
    // The instant has come (the loop's `due`): the coordinated path applies it.
    c.apply_date_step(500_000, StepKind::Coordinated, seq);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d + 500_000));
    assert_eq!(
        c.ntp_step_times.len(),
        storm_before,
        "a micro-correction is not an NTP step-storm step"
    );
    assert_eq!(c.date_sync.last_step.map(|s| s.2), Some("micro"));
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_micro_last_us, Some(500));
    assert_eq!(st.last_date_step_kind, "micro");
}

#[test]
fn a_large_coordinated_step_still_counts_toward_the_step_storm_119() {
    let mut clock = MockSystemClock::new();
    clock.expect_step_clock().returning(|_, _| Ok(()));
    let (mut c, _d) = anchored_controller(clock, MockNtpSource::new(), true);
    let storm_before = c.ntp_step_times.len();
    c.apply_date_step(160_000_000, StepKind::Coordinated, 2);
    assert_eq!(c.ntp_step_times.len(), storm_before + 1);
    assert_eq!(c.date_sync.last_step.map(|s| s.2), Some("coordinated"));
    assert_eq!(c.date_sync.last_micro_ns, None);
}

#[test]
fn the_next_micro_correction_waits_for_the_interval_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(7_000, 1)));
    let (mut c, _d) = anchored_controller(MockSystemClock::new(), ntp, true);
    readings_then_tick(&mut c);
    assert_eq!(c.date_sync.authority.as_ref().unwrap().seq(), 2);
    // Every loop iteration asks again: nothing more inside the 20 s interval.
    for _ in 0..100 {
        c.service_date_offset();
    }
    assert_eq!(c.date_sync.authority.as_ref().unwrap().seq(), 2);
}

#[test]
fn a_follower_takes_a_micro_step_from_the_reply_quietly_and_outside_the_storm_count_119() {
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sign| *dur == Duration::from_micros(500) && *sign == 1)
        .returning(|_, _| Ok(()));
    let (mut c, d) = anchored_controller(clock, MockNtpSource::new(), false);
    let slot = with_authority(&mut c);
    // Aligned with the master's D (seq 1) …
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, 0, 1));
    c.service_date_offset();
    assert!(c.date_sync.follower.adopted());
    // … then its micro step, 10 s ahead.
    let now_ptp = wall_now_ns() - d;
    let mut reply = authority_reply(2, PL_GM, d + 500_000, now_ptp + 10_000_000_000, 2);
    reply.ext.as_mut().unwrap().announce.micro = true;
    *slot.lock().unwrap() = Some(reply);
    c.service_date_offset();
    c.update_shared_status();
    assert!(c.date_sync.follower.is_micro_seq(2));
    {
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert!(st.date_micro_active);
        assert!(st.date_offset_micro, "the follower mirrors the micro kind");
        assert_eq!(st.date_step_pending_ns, Some(500_000));
    }
    let storm_before = c.ntp_step_times.len();
    c.apply_date_step(500_000, StepKind::Coordinated, 2);
    assert_eq!(c.ntp_step_times.len(), storm_before);
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_micro_last_us, Some(500));
    assert_eq!(st.date_correction_rate_ms_per_min, None, "master only");
}

#[test]
fn an_error_the_micro_corrections_cannot_hold_raises_the_falling_behind_alarm_119() {
    // 15 ms off UTC (beyond the 10 ms alarm level, inside the 100 ms cap): corrected only in
    // micro-corrections, and loudly reported as falling behind.
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(15_000, 1)));
    let (mut c, _d) = anchored_controller(MockSystemClock::new(), ntp, true);
    readings_then_tick(&mut c);
    assert!(
        c.date_sync.falling_behind_logged,
        "the loud line was logged"
    );
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert!(st.date_correction_falling_behind);
    assert_eq!(
        st.date_step_pending_ns,
        Some(500_000),
        "still only a micro-correction, never a large step"
    );
}

#[test]
fn stopped_utc_readings_pause_the_micro_corrections_loudly_and_resume_119() {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(|| Ok(one_offset(7_000, 1)));
    // No step_clock expectation: a pause never steps anything.
    let (mut c, d) = anchored_controller(MockSystemClock::new(), ntp, true);
    readings_then_tick(&mut c);
    assert!(!c.date_sync.micro_paused_logged);
    // The master's PTP time 61 s on with no reading since (the anchor moved back by as much).
    c.date_sync.core.set_anchor(d - 61_000_000_000);
    c.service_date_offset();
    assert!(c.date_sync.micro_paused_logged, "the loud line was logged");
    {
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert!(st.date_micro_paused);
    }
    // Back to the time of the last reading: resumed.
    c.date_sync.core.set_anchor(d);
    c.service_date_offset();
    assert!(!c.date_sync.micro_paused_logged);
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert!(!st.date_micro_paused);
}
