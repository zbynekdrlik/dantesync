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

/// The nightly decision to step `amount_ns`, decided on an estimated error of `error_ns`.
fn step(amount_ns: i64, error_ns: i64) -> DailyDecision {
    DailyDecision::Step {
        amount_ns,
        error_ns,
    }
}

/// A 1 ms quantum: a whole-ms error is stepped as it is, so the window tests read the decision
/// directly (the default 200 ms rounding has its own tests).
fn ms_quantum() -> DailyConfig {
    DailyConfig::default().with_step_quantum_ms(1)
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
fn one_rounded_step_is_made_when_the_window_opens_119() {
    let mut d = DailyScheduler::new(DailyConfig::default());
    assert_eq!(
        d.decide(at(1, 59, 59), est(1_500 * MS)),
        DailyDecision::Idle
    );
    assert_eq!(
        d.decide(at(2, 0, 0), est(1_520 * MS)),
        step(1_600 * MS, 1_520 * MS)
    );
    // Once per UTC day: the rest of the window, and the rest of the day, is idle.
    for t in [at(2, 0, 1), at(2, 29, 59), at(2, 31, 0), at(23, 0, 0)] {
        assert_eq!(d.decide(t, est(900 * MS)), DailyDecision::Idle);
    }
    // The next night steps again, backward too.
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(-1_250 * MS)),
        step(-1_200 * MS, -1_250 * MS)
    );
}

#[test]
fn a_small_error_is_left_alone_119() {
    // The dead band is judged on the UNROUNDED error (a 1 ms quantum here, so a step past it is
    // visible).
    let mut d = DailyScheduler::new(ms_quantum());
    // 2 ms + 3 σ (σ = 50 µs) = 2.15 ms.
    assert_eq!(
        d.decide(at(2, 0, 0), est(2_100_000)),
        DailyDecision::NoStep {
            error_ns: 2_100_000
        }
    );
    assert_eq!(d.decide(at(2, 1, 0), est(9 * MS)), DailyDecision::Idle);
    let mut d = DailyScheduler::new(ms_quantum());
    assert_eq!(
        d.decide(at(2, 0, 0), est(-2_200_000)),
        step(-2 * MS, -2_200_000)
    );
}

#[test]
fn the_nightly_step_is_rounded_to_the_nearest_200_ms_ties_away_from_zero_119() {
    // dantesync#119 follow-up (design 6028569753): the step is a whole number of 200 ms — whole
    // frames at 25/30/50/60 fps and whole 48 kHz samples on every per-second grid. The first two
    // are the live 6.10.2026 step (+1543.16 ms = 92.59 frames at 60 fps) and the 7.10.2026 error.
    for (error, amount) in [
        (1_543_161_209, 1_600 * MS),
        (1_489_638_000, 1_400 * MS),
        (1_400 * MS, 1_400 * MS),
        (299_999_999, 200 * MS),
        (100_000_001, 200 * MS),
        // Exactly half a quantum: away from zero.
        (100 * MS, 200 * MS),
        (300 * MS, 400 * MS),
        (1_500 * MS, 1_600 * MS),
        // Negative (the fleet ahead of UTC): the same rule, mirrored.
        (-1_543_161_209, -1_600 * MS),
        (-1_489_638_000, -1_400 * MS),
        (-299_999_999, -200 * MS),
        (-100 * MS, -200 * MS),
        (-300 * MS, -400 * MS),
        (-1_500 * MS, -1_600 * MS),
    ] {
        let mut d = DailyScheduler::new(DailyConfig::default());
        assert_eq!(
            d.decide(at(2, 0, 0), est(error)),
            step(amount, error),
            "error {error}"
        );
    }
    // No overflow at the extremes: still a whole, signed multiple of the quantum.
    for error in [i64::MAX, -i64::MAX] {
        let mut d = DailyScheduler::new(DailyConfig::default());
        let DailyDecision::Step { amount_ns, .. } = d.decide(at(2, 0, 0), est(error)) else {
            panic!("a step for {error}");
        };
        assert_eq!(amount_ns % (200 * MS), 0, "{error}: {amount_ns}");
        assert_eq!(amount_ns.signum(), error.signum(), "{error}: {amount_ns}");
    }
}

#[test]
fn a_step_that_rounds_to_zero_is_no_step_and_the_next_night_measures_it_again_119() {
    // Past the dead band but under half a quantum: nothing is stepped tonight, the night is
    // handled, and the remainder is measured again the next night with a day's drift on top.
    for (error, next_night) in [
        (99_999_999, 1_600 * MS),
        (-99_999_999, 1_400 * MS),
        (50 * MS, 1_600 * MS),
        (2_200_000, 1_600 * MS),
        (-2_200_000, 1_600 * MS),
    ] {
        let mut d = DailyScheduler::new(DailyConfig::default());
        assert_eq!(
            d.decide(at(2, 0, 0), est(error)),
            DailyDecision::NoStep { error_ns: error },
            "error {error}"
        );
        assert_eq!(d.decide(at(2, 10, 0), est(error)), DailyDecision::Idle);
        let tomorrow = error + 1_520 * MS;
        assert_eq!(
            d.decide(at(2, 0, 0) + 86_400 * S, est(tomorrow)),
            step(next_night, tomorrow),
            "error {error}"
        );
    }
}

#[test]
fn a_configured_quantum_rounds_to_its_own_multiple_119() {
    for (quantum_ms, error, amount) in [
        (1_000, 1_543_161_209, 2_000 * MS),
        (1_000, 1_489_638_000, 1_000 * MS),
        (1_000, -1_500 * MS, -2_000 * MS),
        (40, 1_543_161_209, 1_560 * MS),
        (40, -1_543_161_209, -1_560 * MS),
        (1, 1_543_161_209, 1_543 * MS),
    ] {
        let mut d = DailyScheduler::new(DailyConfig::default().with_step_quantum_ms(quantum_ms));
        assert_eq!(
            d.decide(at(2, 0, 0), est(error)),
            step(amount, error),
            "quantum {quantum_ms} ms, error {error}"
        );
    }
}

#[test]
fn the_step_quantum_is_200_ms_by_default_and_must_divide_a_second_119() {
    assert_eq!(DEFAULT_DAILY_STEP_QUANTUM_MS, 200);
    assert_eq!(DailyConfig::default().step_quantum_ns, 200 * MS);
    assert_eq!(DailyConfig::new(7_200, 0).step_quantum_ns, 200 * MS);
    // 0 = the default; a divisor of 1000 ms is taken; anything else is refused (the caller warns
    // and uses the default).
    for (raw, want) in [
        (0, Some(200)),
        (1, Some(1)),
        (40, Some(40)),
        (125, Some(125)),
        (200, Some(200)),
        (500, Some(500)),
        (1_000, Some(1_000)),
        (3, None),
        (300, None),
        (999, None),
        (2_000, None),
        (u64::MAX, None),
    ] {
        assert_eq!(daily_step_quantum_ms(raw), want, "{raw}");
        assert_eq!(
            DailyConfig::default()
                .with_step_quantum_ms(raw)
                .step_quantum_ms(),
            want.unwrap_or(200),
            "{raw}"
        );
    }
}

#[test]
fn without_utc_the_window_waits_up_to_30_minutes_then_skips_the_night_119() {
    let mut d = DailyScheduler::new(ms_quantum());
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
        step(1_400 * MS, 1_400 * MS)
    );

    let mut d = DailyScheduler::new(ms_quantum());
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
        step(2_900 * MS, 2_900 * MS),
        "the next night steps two days' error"
    );
}

#[test]
fn a_window_that_closed_while_the_scheduler_was_not_asked_is_reported_once_119() {
    // The authority only asks while nothing else is in flight. A window that opened and closed in
    // between (another date change in flight all along, a stalled process) is missed: reported
    // once, loudly (review round 1) — only a window already past at the FIRST ask (a boot) is
    // silent.
    let mut d = DailyScheduler::new(ms_quantum());
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
        step(900 * MS, 900 * MS)
    );
}

#[test]
fn an_emergency_step_back_across_a_night_never_hides_the_next_window_119() {
    // Review round 2: a master booted 6 h ahead (fleet wall 04:00) marks the 02:00 window it
    // first sees as handled; the emergency step back to 22:00 must not make the next 02:00 look
    // handled too — nor report the night it jumped over as missed.
    let mut d = DailyScheduler::new(ms_quantum());
    assert_eq!(d.decide(at(4, 0, 0), None), DailyDecision::Idle);
    d.on_emergency_step(at(4, 0, 5) - 6 * 3_600 * S);
    for t in [at(22, 0, 5) - 86_400 * S, at(23, 0, 0) - 86_400 * S] {
        assert_eq!(d.decide(t, est(MS * 50)), DailyDecision::Idle, "{t}");
    }
    assert_eq!(
        d.next_window_wall_ns(at(23, 0, 0) - 86_400 * S),
        at(2, 0, 0)
    );
    assert_eq!(
        d.decide(at(2, 0, 0), est(MS * 70)),
        step(70 * MS, 70 * MS),
        "the next night steps"
    );
    // A FORWARD emergency jump over a window is not a missed night either (it just corrected
    // the whole error): silent, and the next window still steps.
    let mut d = DailyScheduler::new(ms_quantum());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    d.on_emergency_step(at(1, 0, 5) + 3 * 3_600 * S);
    assert_eq!(d.decide(at(4, 0, 5), est(MS * 5)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(2, 0, 0) + 86_400 * S, est(MS * 90)),
        step(90 * MS, 90 * MS)
    );
    // An emergency step that lands INSIDE an open window leaves that window to decide.
    let mut d = DailyScheduler::new(ms_quantum());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    d.on_emergency_step(at(2, 5, 0));
    assert_eq!(d.decide(at(2, 5, 0), est(MS * 30)), step(30 * MS, 30 * MS));
}

#[test]
fn a_backward_step_across_the_window_start_never_decides_the_night_again_119() {
    // Review round 1: a step lands the fleet wall BEFORE the window start it was decided in (a
    // backward step larger than the landing's distance from the start, possible once
    // daily_emergency_ms is configured past 10 s). The night is handled: no second decision, no
    // false skip, and the next window is tomorrow's.
    let mut d = DailyScheduler::new(ms_quantum());
    assert_eq!(d.decide(at(1, 0, 0), None), DailyDecision::Idle);
    assert_eq!(d.decide(at(2, 0, 0), est(-20 * S)), step(-20 * S, -20 * S));
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
        step(900 * MS, 900 * MS)
    );
}

#[test]
fn a_boot_inside_the_window_steps_when_the_estimate_exists_119() {
    let mut d = DailyScheduler::new(ms_quantum());
    assert!(matches!(
        d.decide(at(2, 15, 0), None),
        DailyDecision::Waiting { .. }
    ));
    assert_eq!(d.decide(at(2, 16, 0), est(40 * MS)), step(40 * MS, 40 * MS));
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
    let mut d = DailyScheduler::new(DailyConfig::new(0, 0).with_step_quantum_ms(1));
    assert_eq!(d.decide(at(23, 59, 0), est(900 * MS)), DailyDecision::Idle);
    assert_eq!(
        d.decide(DAY0 + 86_400 * S, est(900 * MS)),
        step(900 * MS, 900 * MS)
    );
    let mut d = DailyScheduler::new(
        DailyConfig::new(parse_daily_step_utc("13:45:30").unwrap(), 0).with_step_quantum_ms(1),
    );
    assert_eq!(d.decide(at(13, 45, 29), est(900 * MS)), DailyDecision::Idle);
    assert_eq!(
        d.decide(at(13, 45, 30), est(900 * MS)),
        step(900 * MS, 900 * MS)
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
