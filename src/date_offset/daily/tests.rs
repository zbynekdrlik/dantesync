use super::*;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
/// 2026-09-26T00:00:00Z.
const DAY0: i64 = 1_790_380_800 * S;

fn est(error_ns: i64) -> Option<MicroEstimate> {
    Some(MicroEstimate {
        error_ns,
        trend_ns_per_s: 17_600.0,
        noise_ns: 50_000.0,
        trend_noise_ns_per_s: 10.0,
    })
}

fn at(h: i64, m: i64, s: i64) -> i64 {
    DAY0 + (h * 3_600 + m * 60 + s) * S
}

#[test]
fn daily_step_utc_parses_leniently_119() {
    assert_eq!(parse_daily_step_utc("02:00"), Some(7_200));
    assert_eq!(parse_daily_step_utc(" 2:00 "), Some(7_200));
    assert_eq!(parse_daily_step_utc("02:00:30"), Some(7_230));
    assert_eq!(parse_daily_step_utc("23:59:59"), Some(86_399));
    assert_eq!(parse_daily_step_utc("4"), Some(14_400));
    assert_eq!(parse_daily_step_utc("00:00"), Some(0));
    for bad in [
        "",
        "24:00",
        "02:60",
        "02:00:60",
        "2am",
        "02-00",
        "02:00:00:00",
        "-1:00",
        "002:00",
        ":30",
        "02:",
        "٠٢:٠٠",
    ] {
        assert_eq!(parse_daily_step_utc(bad), None, "{bad:?}");
    }
}

#[test]
fn the_emergency_cap_is_clamped_and_zero_means_the_default_119() {
    assert_eq!(clamp_daily_emergency_ms(0), DEFAULT_DAILY_EMERGENCY_MS);
    assert_eq!(clamp_daily_emergency_ms(1), MIN_DAILY_EMERGENCY_MS);
    assert_eq!(clamp_daily_emergency_ms(u64::MAX), MAX_DAILY_EMERGENCY_MS);
    assert_eq!(clamp_daily_emergency_ms(7_000), 7_000);
    assert_eq!(DailyConfig::default().emergency_ns, 5_000 * MS);
    assert_eq!(DailyConfig::default().step_tod_ns, 7_200 * S);
    // A time of day is taken modulo one day.
    assert_eq!(DailyConfig::new(86_400 + 60, 0).step_tod_ns, 60 * S);
}

#[test]
fn rfc3339_formats_the_utc_second_119() {
    assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(format_utc_rfc3339(at(2, 0, 0)), "2026-09-26T02:00:00Z");
    assert_eq!(
        format_utc_rfc3339(at(2, 0, 0) + 999 * MS),
        "2026-09-26T02:00:00Z"
    );
    assert_eq!(format_utc_rfc3339(951_782_400 * S), "2000-02-29T00:00:00Z");
    assert_eq!(
        format_utc_rfc3339(4_107_542_399 * S),
        "2100-02-28T23:59:59Z"
    );
    assert_eq!(format_utc_rfc3339(-1), "1969-12-31T23:59:59Z");
}

#[test]
fn nothing_is_decided_during_the_day_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    // A boot in the afternoon: this night's window is past and is handled silently.
    for t in [
        at(14, 0, 0),
        at(20, 0, 0),
        at(23, 59, 59),
        at(1, 59, 59) + 86_400 * S,
    ] {
        assert_eq!(d.decide(t, est(700 * MS)), DailyDecision::Idle, "{t}");
    }
}

#[test]
fn the_whole_error_is_stepped_once_when_the_window_opens_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(
        d.decide(at(1, 59, 59), est(1_500 * MS)),
        DailyDecision::Idle
    );
    assert_eq!(
        d.decide(at(2, 0, 0), est(1_520 * MS)),
        DailyDecision::Step {
            amount_ns: 1_520 * MS
        }
    );
    // Once per UTC day: the rest of the window, and the rest of the day, is idle.
    for t in [at(2, 0, 1), at(2, 29, 59), at(2, 31, 0), at(23, 0, 0)] {
        assert_eq!(d.decide(t, est(900 * MS)), DailyDecision::Idle);
    }
    // The next night steps again, backward too.
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(-1_300 * MS)),
        DailyDecision::Step {
            amount_ns: -1_300 * MS
        }
    );
}

#[test]
fn a_small_error_is_left_alone_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    // 2 ms + 3 σ (σ = 50 µs) = 2.15 ms.
    assert_eq!(
        d.decide(at(2, 0, 0), est(2_100_000)),
        DailyDecision::NoStep {
            error_ns: 2_100_000
        }
    );
    assert_eq!(d.decide(at(2, 1, 0), est(9 * MS)), DailyDecision::Idle);
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(
        d.decide(at(2, 0, 0), est(-2_200_000)),
        DailyDecision::Step {
            amount_ns: -2_200_000
        }
    );
}

#[test]
fn without_utc_the_window_waits_up_to_30_minutes_then_skips_the_night_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(2, 0, 0), None),
        DailyDecision::Waiting {
            window_end_wall_ns: at(2, 30, 0)
        }
    );
    // Reported once.
    assert_eq!(d.decide(at(2, 10, 0), None), DailyDecision::Idle);
    // UTC back inside the window: the step is made then.
    assert_eq!(
        d.decide(at(2, 29, 59), est(1_400 * MS)),
        DailyDecision::Step {
            amount_ns: 1_400 * MS
        }
    );

    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    assert!(matches!(
        d.decide(at(2, 0, 0), None),
        DailyDecision::Waiting { .. }
    ));
    assert_eq!(
        d.decide(at(2, 30, 0), est(1_400 * MS)),
        DailyDecision::Skipped {
            next_window_wall_ns: at(2, 0, 0) + 86_400 * S
        },
        "UTC came back after the window: the night is skipped"
    );
    assert_eq!(d.decide(at(3, 0, 0), est(1_400 * MS)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(2_900 * MS)),
        DailyDecision::Step {
            amount_ns: 2_900 * MS
        },
        "the next night steps two days' error"
    );
}

#[test]
fn a_window_that_closed_while_the_scheduler_was_not_asked_is_reported_once_119() {
    // The authority only asks while nothing else is in flight. A window that opened and closed in
    // between (another date change in flight all along, a stalled process) is missed: reported
    // once, loudly (review round 1) — only a window already past at the FIRST ask (a boot) is
    // silent.
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(d.decide(at(1, 0, 0), est(MS * 900)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(3, 0, 0), est(MS * 900)),
        DailyDecision::Missed {
            next_window_wall_ns: at(2, 0, 0) + 86_400 * S
        }
    );
    assert_eq!(d.decide(at(3, 0, 1), est(MS * 900)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(MS * 900)),
        DailyDecision::Step {
            amount_ns: 900 * MS
        }
    );
}

#[test]
fn a_backward_step_across_the_window_start_never_decides_the_night_again_119() {
    // Review round 1: a step lands the fleet wall BEFORE the window start it was decided in (a
    // backward step larger than the landing's distance from the start, possible once
    // daily_emergency_ms is configured past 10 s). The night is handled: no second decision, no
    // false skip, and the next window is tomorrow's.
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(2, 0, 0), est(-20 * S)),
        DailyDecision::Step { amount_ns: -20 * S }
    );
    // Landed at 02:00:10: the wall reads 01:59:50.
    for t in [
        at(1, 59, 50),
        at(1, 59, 59),
        at(2, 0, 0),
        at(2, 10, 0),
        at(2, 30, 0),
    ] {
        assert_eq!(d.decide(t, None), DailyDecision::Idle, "{t}");
        assert_eq!(
            d.next_window_wall_ns(t),
            at(2, 0, 0) + 86_400 * S,
            "the next window is tomorrow's"
        );
    }
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(MS * 900)),
        DailyDecision::Step {
            amount_ns: 900 * MS
        }
    );
}

#[test]
fn a_boot_inside_the_window_steps_when_the_estimate_exists_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert!(matches!(
        d.decide(at(2, 15, 0), None),
        DailyDecision::Waiting { .. }
    ));
    assert_eq!(
        d.decide(at(2, 16, 0), est(40 * MS)),
        DailyDecision::Step { amount_ns: 40 * MS }
    );
}

#[test]
fn the_next_window_is_the_open_one_until_it_is_handled_119() {
    let cfg = DailyConfig::new(parse_daily_step_utc("02:00").unwrap(), 0);
    let mut d = DailyScheduler::new(cfg);
    assert_eq!(d.next_window_wall_ns(at(1, 0, 0)), at(2, 0, 0));
    assert_eq!(
        d.next_window_wall_ns(at(2, 10, 0)),
        at(2, 0, 0),
        "still open"
    );
    assert_eq!(
        d.next_window_wall_ns(at(2, 30, 0)),
        at(2, 0, 0) + 86_400 * S
    );
    assert!(matches!(
        d.decide(at(2, 0, 0), est(MS * 800)),
        DailyDecision::Step { .. }
    ));
    d.record_step(at(2, 0, 10), 800 * MS);
    assert_eq!(d.last_step(), Some((at(2, 0, 10), 800 * MS)));
    assert_eq!(d.next_window_wall_ns(at(2, 0, 1)), at(2, 0, 0) + 86_400 * S);
}

#[test]
fn a_window_at_midnight_and_a_custom_time_work_119() {
    let mut d = DailyScheduler::new(DailyConfig::new(0, 0));
    assert_eq!(d.decide(at(23, 59, 0), est(900 * MS)), DailyDecision::Idle);
    assert_eq!(
        d.decide(DAY0 + 86_400 * S, est(900 * MS)),
        DailyDecision::Step {
            amount_ns: 900 * MS
        }
    );
    let mut d = DailyScheduler::new(DailyConfig::new(
        parse_daily_step_utc("13:45:30").unwrap(),
        0,
    ));
    assert_eq!(d.decide(at(13, 45, 29), est(900 * MS)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(13, 45, 30), est(900 * MS)),
        DailyDecision::Step {
            amount_ns: 900 * MS
        }
    );
}

#[test]
fn the_mode_labels_are_the_config_values_119() {
    assert_eq!(
        CorrectionMode::Daily(DailyConfig::default()).label(),
        "daily"
    );
    assert_eq!(CorrectionMode::Micro.label(), "micro");
}
