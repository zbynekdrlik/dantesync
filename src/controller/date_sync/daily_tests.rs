//! dantesync#119 (1.12) — the controller wiring of the NIGHTLY date step: the configured mode
//! reaches the master's authority, the nightly step is pre-announced (DSYX, two leads) and shown
//! in `/status` during its lead — signed size and due time, the SongPlayer requirement — then
//! cleared once it lands; nothing is announced by day, an error beyond the emergency cap is
//! stepped at once, and a master without PTP keeps off the local NTP step path. The decision is
//! proven in `crate::date_offset::daily` and end-to-end in `tests/two_clock_bench/daily.rs`.

use super::tests::{anchored_controller_with, one_offset, readings_then_tick};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::MockNtpSource;

const S: i64 = 1_000_000_000;

/// A phase-lock config in DAILY mode whose nightly window opens at the wall second `start_s`'s
/// time of day.
fn daily_config(start_s: i64) -> SystemConfig {
    let tod = start_s.rem_euclid(86_400);
    let mut config = super::tests::phase_lock_config();
    config.date_offset.correction = "daily".to_string();
    config.date_offset.daily_step_utc =
        format!("{:02}:{:02}:{:02}", tod / 3_600, tod % 3_600 / 60, tod % 60);
    config
}

/// The wall second now.
fn now_s() -> i64 {
    wall_now_ns() / S
}

fn ntp_at(us: i64) -> MockNtpSource {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(move || Ok(one_offset(us.abs(), if us < 0 { -1 } else { 1 })));
    ntp
}

#[test]
fn the_nightly_step_is_pre_announced_and_shown_in_status_through_its_lead_then_cleared_119() {
    for (us, sign) in [(300_000_i64, 1_i8), (-300_000, -1)] {
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .withf(move |dur, sg| *dur == Duration::from_millis(300) && *sg == sign)
            .returning(|_, _| Ok(()));
        // The window opened a second ago: the step is decided at the first loop iteration with
        // an estimate (six readings).
        let start_s = now_s() - 1;
        let (mut c, d) = anchored_controller_with(clock, ntp_at(us), true, daily_config(start_s));
        readings_then_tick(&mut c);
        let delta = us * 1_000;
        let seq = c.date_sync.authority.as_ref().unwrap().seq();
        assert_eq!(seq, 2, "one announce");
        {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            // The pre-announce consumers (SongPlayer) hold or mark the event on.
            assert_eq!(
                st.date_step_pending_ns,
                Some(delta),
                "the whole error, signed"
            );
            let due = st.date_step_due_in_ms.expect("due time during the lead");
            assert!((9_000..=10_000).contains(&due), "two 5 s leads: {due} ms");
            assert!(!st.date_offset_micro, "not a micro-correction");
            assert!(!st.date_slew_active && st.date_slew_ppm.is_none(), "a STEP");
            assert_eq!(st.date_offset_ns, Some(d), "in effect only at the instant");
            assert_eq!(st.date_correction_mode, "daily");
            assert_eq!(st.date_daily_last_step_ms, Some(delta as f64 / 1e6));
            let landing = (wall_now_ns() + 10 * S) / S;
            let ts = st
                .date_daily_last_step_ts
                .expect("the step's landing second");
            assert!(ts.abs_diff(landing as u64) <= 2, "{ts} vs {landing}");
            // This night's window is handled: the next one is tomorrow's.
            assert_eq!(
                st.date_daily_next_utc,
                Some(crate::date_offset::format_utc_rfc3339(
                    (start_s + 86_400) * S
                ))
            );
            assert!(!st.date_correction_falling_behind);
        }
        // The instant has come (the loop's `due`): the coordinated path applies it.
        let due = c
            .date_sync
            .follower
            .due(wall_now_ns() + 11 * S)
            .expect("scheduled on the master's own wall");
        assert_eq!(due.delta_ns, delta);
        c.apply_date_step(due.delta_ns, StepKind::Coordinated, due.seq);
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert_eq!(st.date_step_pending_ns, None, "cleared once it landed");
        assert_eq!(st.date_step_due_in_ms, None);
        assert_eq!(st.date_offset_ns, Some(d + delta));
        assert_eq!(st.last_date_step_ns, Some(delta));
        assert_eq!(st.last_date_step_kind, "coordinated");
    }
}

#[test]
fn a_daily_master_announces_nothing_by_day_and_raises_no_micro_alarm_119() {
    // 300 ms off UTC (30 × the micro falling-behind level), the window 6 h away: nothing.
    let start_s = now_s() + 6 * 3_600;
    let (mut c, _d) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(300_000),
        true,
        daily_config(start_s),
    );
    readings_then_tick(&mut c);
    for _ in 0..100 {
        c.service_date_offset();
    }
    assert_eq!(c.date_sync.authority.as_ref().unwrap().seq(), 1);
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_step_pending_ns, None);
    assert_eq!(st.date_correction_mode, "daily");
    assert!(!st.date_correction_falling_behind, "false in daily mode");
    assert!(!st.date_micro_paused);
    assert_eq!(st.date_offset_error_ms, Some(300.0));
    assert_eq!(st.date_daily_last_step_ts, None);
    assert_eq!(st.date_daily_last_step_ms, None);
    assert_eq!(
        st.date_daily_next_utc,
        Some(crate::date_offset::format_utc_rfc3339(start_s * S))
    );
}

#[test]
fn an_error_beyond_the_emergency_cap_is_stepped_at_once_in_daily_mode_119() {
    let (mut c, _d) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(6_000_000),
        true,
        daily_config(now_s() + 6 * 3_600),
    );
    for _ in 0..2 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    assert_eq!(st.date_step_pending_ns, Some(6_000_000_000));
    let due = st.date_step_due_in_ms.expect("scheduled");
    assert!((4_000..=5_000).contains(&due), "one 5 s lead: {due} ms");
    assert_eq!(
        st.date_daily_last_step_ms, None,
        "an emergency is not the nightly step"
    );
}

#[test]
fn a_daily_master_without_ptp_keeps_off_the_local_ntp_step_path_119() {
    // In daily mode the fleet line is deliberately off UTC (up to a day's drift): stepping the
    // master's own wall to UTC would move it that far off the fleet (and back at its re-join).
    // No step_clock expectation: nothing is stepped.
    let (mut c, d) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(300_000),
        true,
        daily_config(now_s() + 6 * 3_600),
    );
    c.ptp_offline = true;
    c.service_date_offset();
    for _ in 0..4 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d),
        "its own wall did not move"
    );
    assert!(c.ntp_pending_step.is_none());
    assert_eq!(
        c.date_sync.master_utc_error_ns,
        Some(300_000_000),
        "the fleet line's error is still fed"
    );
}

#[test]
fn a_daily_master_without_ptp_takes_its_own_nightly_step_with_the_fleet_119() {
    // Review round 1: its D is the fleet D (it takes no local NTP steps), so it schedules the
    // nightly step on its own wall like every follower — a long outage never leaves it a day's
    // drift off the fleet.
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sg| *dur == Duration::from_millis(300) && *sg == 1)
        .returning(|_, _| Ok(()));
    let start_s = now_s() - 1;
    let (mut c, d) = anchored_controller_with(clock, ntp_at(300_000), true, daily_config(start_s));
    c.ptp_offline = true;
    c.service_date_offset();
    readings_then_tick(&mut c);
    assert_eq!(
        c.date_sync.authority.as_ref().unwrap().seq(),
        2,
        "announced"
    );
    let due = c
        .date_sync
        .follower
        .due(wall_now_ns() + 11 * S)
        .expect("the off-line master scheduled the fleet step for its own wall");
    assert_eq!(due.delta_ns, 300_000_000);
    c.apply_date_step(due.delta_ns, StepKind::Coordinated, due.seq);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d + 300_000_000));
}

#[test]
fn an_off_line_daily_masters_failed_step_is_retried_after_the_backoff_119() {
    // Review round 2: with no PTP there is no re-alignment window, so a failed own step would
    // leave the master a whole nightly step off the fleet until PTP returns. It re-joins the
    // fleet line after the backoff (there is no phase error to measure without PTP).
    //
    // A failed step leaves the master's D behind the fleet D (the authority's, in effect): the
    // test models that directly — its anchor 300 ms behind the fleet's, and the failure's
    // backoff running — because no time passes in a controller test (the announce's instant
    // cannot be reached).
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sg| *dur == Duration::from_millis(300) && *sg == 1)
        .returning(|_, _| Ok(()));
    let (mut c, d) = anchored_controller_with(
        clock,
        MockNtpSource::new(),
        true,
        daily_config(now_s() + 6 * 3_600),
    );
    c.ptp_offline = true;
    c.service_date_offset();
    c.date_sync.core.set_anchor(d - 300_000_000);
    c.date_sync.step_failed_at = Some(Instant::now());
    // Inside the backoff nothing happens; after it, one re-join onto the fleet line.
    c.service_date_offset();
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 300_000_000));
    c.date_sync.step_failed_at = Some(Instant::now() - Duration::from_secs(11));
    c.service_date_offset();
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d),
        "back on the fleet line"
    );
    assert_eq!(c.date_sync.last_step.map(|s| s.2), Some("join"));
    // … and a micro-mode master without PTP keeps the local NTP path instead (no re-join here).
    let (mut c, d) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        true,
        super::tests::phase_lock_config(),
    );
    c.ptp_offline = true;
    c.service_date_offset();
    c.date_sync.core.set_anchor(d - 300_000_000);
    c.service_date_offset();
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d - 300_000_000));
}

#[test]
fn the_configured_mode_reaches_the_authority_and_micro_stays_micro_119() {
    let (c, _d) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        true,
        daily_config(now_s()),
    );
    assert!(matches!(
        c.date_sync.authority.as_ref().unwrap().correction_mode(),
        crate::date_offset::CorrectionMode::Daily(_)
    ));
    let (c, _d) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        true,
        super::tests::phase_lock_config(),
    );
    assert_eq!(
        c.date_sync.authority.as_ref().unwrap().correction_mode(),
        crate::date_offset::CorrectionMode::Micro,
        "the pre-1.12 tests run the micro mode"
    );
}
