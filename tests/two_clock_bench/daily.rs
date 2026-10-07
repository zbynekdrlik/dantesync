//! dantesync#119 (1.12) — the bench's DAILY scenarios: the fleet date corrected by ONE coordinated
//! step per night (owner decision, issue 119 comment 5849932587).
//!
//! Proven over 48 simulated hours (the grandmaster change and reboot still happen):
//! - at +17.6 ppm (the rig): exactly two steps, both announced when the 02:00 UTC window opens,
//!   none outside it; each the error rounded to a whole 200 ms (1.16: whole frames and samples on
//!   every per-second grid), the remainder carried into the next night; every box within 50 µs of
//!   relative phase; no wall ever running back;
//! - at −15 ppm: the fleet runs ahead, and it is stepped BACK at night, never slewed by day;
//! - a 6 s UTC jump (beyond the 5 s emergency cap) is stepped at once;
//! - a UTC outage over the window: the step is made when UTC returns inside the 30 min window,
//!   and otherwise the night is skipped and the next one steps both days' error.
//!
//! The bench starts at 14:13:20 UTC (`base_utc`), so the windows open 42 400 s and 128 800 s in
//! (true time ≈ fleet time here: the fleet runs at grandmaster A's rate, 0 ppm).

use super::*;

/// Where each nightly window opens, in 0.5 s windows from the start.
const FIRST_WINDOW_W: u64 = 42_400 * 2;
const SECOND_WINDOW_W: u64 = 128_800 * 2;
/// The step lands two 5 s leads after the window opens.
const LANDING_S: f64 = 10.0;
/// dantesync#119 (1.16): every nightly step is a whole multiple of the default quantum, so the
/// fleet line lands within half of it of UTC (the remainder waits for the next night).
const QUANTUM: i64 = 200 * MS;
/// The master's estimate of the error is this close to the true error (the bench's NTP noise
/// through the robust line).
const ESTIMATE_TOL: i64 = 5 * MS;

/// `step` is a whole number of quanta, the nearest to the true error `error` (to within the
/// estimate's tolerance).
fn assert_rounded_step(label: &str, what: &str, step: i64, error: i64) {
    assert_eq!(
        step % QUANTUM,
        0,
        "[{label}] {what} {step} is not a whole number of 200 ms"
    );
    assert!(
        (step - error).abs() <= QUANTUM / 2 + ESTIMATE_TOL,
        "[{label}] {what} {step} is not the error {error} rounded to 200 ms"
    );
}

fn daily_scenario(label: &'static str, utc_vs_gm_ppm: f64) -> Scenario {
    let mut sc = Scenario::plain(label, utc_vs_gm_ppm, true);
    sc.correction = CorrectionMode::Daily(DailyConfig::default());
    sc.run_windows = 48 * 3_600 * 2;
    // UTC drifts at the same rate against grandmaster B as against A.
    sc.gm_b_ppm = GM_A_PPM;
    sc
}

/// The fleet-wall time of day (s) of each correction's announce.
fn announce_tods_s(r: &RunResult) -> Vec<f64> {
    r.correction_walls
        .iter()
        .map(|w| w.rem_euclid(86_400 * S) as f64 / 1e9)
        .collect()
}

/// Run the scenarios in parallel (each is 48 simulated hours).
fn run_all(scenarios: &[Scenario]) -> Vec<RunResult> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = scenarios
            .iter()
            .map(|sc| scope.spawn(move || run(sc)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a bench run panicked"))
            .collect()
    })
}

/// The true UTC-vs-fleet error accrued over `seconds` at `ppm`, ns.
fn accrued(ppm: f64, seconds: f64) -> i64 {
    (ppm * seconds * 1_000.0).round() as i64
}

fn assert_in_window(label: &str, tods: &[f64], opens_s: f64) {
    for &tod in tods {
        assert!(
            (opens_s..opens_s + 1.0).contains(&tod),
            "[{label}] a correction announced at {tod:.1} s of the UTC day, not when the window \
             opens ({opens_s} s): {tods:?}"
        );
    }
}

#[test]
fn a_day_at_the_rigs_drift_is_one_step_a_night_in_the_window_and_none_by_day_119() {
    let scenarios = [
        daily_scenario("daily: UTC +17.6 ppm vs GM, 48 h", 17.6),
        daily_scenario("daily: UTC -15 ppm vs GM, 48 h", -15.0),
    ];
    let results = run_all(&scenarios);
    for (sc, r) in scenarios.iter().zip(&results) {
        let label = sc.label;
        check(sc, r);
        let tods = announce_tods_s(r);
        println!(
            "[{label}] corrections {:?} announced at {tods:?} s of the UTC day; relative phase max \
             {} µs; fleet |UTC − wall| max {} ms; wall ran back {} times",
            r.corrections,
            r.max_relative_phase_ns / US,
            r.max_fleet_utc_error_ns / MS,
            r.wall_went_back
        );
        // Exactly two corrections — one per night — both STEPS, announced when the window opens.
        assert_eq!(r.corrections.len(), 2, "[{label}] {:?}", r.corrections);
        assert_eq!(r.announced.len(), 2, "[{label}] {:?}", r.announced);
        assert!(r.announced_slews.is_empty(), "[{label}] never a slew");
        assert_in_window(label, &tods, 7_200.0);
        let windows: Vec<u64> = r.corrections.iter().map(|c| c.0).collect();
        assert!(
            windows[0].abs_diff(FIRST_WINDOW_W) <= 4 && windows[1].abs_diff(SECOND_WINDOW_W) <= 4,
            "[{label}] the two nights: {windows:?}"
        );
        // Each step is the error where it lands rounded to 200 ms: the first the drift since the
        // start, the second a full day's PLUS the remainder the first left (carried, never lost).
        let ppm = sc.utc_vs_gm_ppm;
        let first = accrued(ppm, FIRST_WINDOW_W as f64 / 2.0 + LANDING_S);
        let second = accrued(ppm, 86_400.0);
        let (c0, c1) = (r.corrections[0].1, r.corrections[1].1);
        let before_second = first - c0 + second;
        println!(
            "[{label}] steps {} ms and {} ms (the true error where they land: {} ms and {} ms)",
            c0 as f64 / 1e6,
            c1 as f64 / 1e6,
            first as f64 / 1e6,
            before_second as f64 / 1e6
        );
        assert_rounded_step(label, "first step", c0, first);
        assert_rounded_step(label, "second step", c1, before_second);
        if ppm > 0.0 {
            assert!(second > 1_500 * MS, "a day at +17.6 ppm is ~1.52 s");
            assert_eq!(r.wall_went_back, 0, "[{label}] forward steps only");
        } else {
            assert!(r.corrections.iter().all(|c| c.1 <= -600 * MS));
            // Only at the two backward steps does any wall read less than a window before.
            assert!(
                r.wall_went_back <= 2 * 6,
                "[{label}] the walls ran back {} times",
                r.wall_went_back
            );
        }
        // The fleet agrees to the µs all along (each wall + its own path delay).
        assert!(
            r.max_relative_phase_ns <= 50 * US,
            "[{label}] relative phase {} µs",
            r.max_relative_phase_ns / US
        );
        // The fleet date ran free by day: its error reached a day's drift (with the first night's
        // remainder), never the emergency cap.
        assert!(
            r.max_fleet_utc_error_ns >= before_second.abs() - 10 * MS
                && r.max_fleet_utc_error_ns < 2 * second.abs(),
            "[{label}] the fleet line was {} ms off UTC at most",
            r.max_fleet_utc_error_ns / MS
        );
    }
}

#[test]
fn a_6_s_utc_jump_is_stepped_at_once_as_an_emergency_119() {
    // UTC jumps 6 s ahead at 17:13:20 (beyond the 5 s emergency cap): two agreeing readings, then
    // one coordinated step, 5 s ahead — not the next night.
    let jump_w = 3 * 3_600 * 2;
    let mut sc = daily_scenario("daily: a 6 s UTC jump by day, 24 h", 17.6);
    sc.run_windows = 24 * 3_600 * 2;
    sc.utc_jumps = vec![(jump_w, 6 * S)];
    let r = run(&sc);
    check(&sc, &r);
    let tods = announce_tods_s(&r);
    println!("[emergency] corrections {:?} at {tods:?} s", r.corrections);
    assert_eq!(r.corrections.len(), 2, "the emergency, then the night");
    assert_eq!(r.wall_went_back, 0, "a forward emergency");
    let (at_w, size) = r.corrections[0];
    assert!(
        (jump_w..jump_w + 3 * NTP_INTERVAL_WINDOWS).contains(&at_w),
        "announced within two readings of the jump: window {at_w}"
    );
    let expected = 6 * S + accrued(17.6, jump_w as f64 / 2.0 + 5.0);
    assert!(
        (size - expected).abs() < 5 * MS,
        "the whole error at once: {size} vs {expected}"
    );
    assert_ne!(size % QUANTUM, 0, "an emergency is never rounded: {size}");
    assert_in_window("emergency, then the night", &tods[1..], 7_200.0);
    assert!(
        r.corrections[1].1.abs() < 700 * MS,
        "the night steps only what drifted since the emergency: {}",
        r.corrections[1].1
    );
    assert_eq!(
        r.corrections[1].1 % QUANTUM,
        0,
        "the night's step is rounded"
    );
    assert!(r.max_relative_phase_ns <= 50 * US);
    assert_eq!(r.wall_went_back, 0);
}

#[test]
fn a_utc_outage_over_the_window_steps_when_utc_returns_else_the_next_night_119() {
    // UTC is lost from 01:55 to 02:10 (fleet time): the window waits, and the step is made at the
    // first fresh reading after 02:10. Then from 01:55 to 03:00: the window closes, the night is
    // skipped, and the next night steps both days' error.
    let mut back = daily_scenario("daily: UTC lost 01:55-02:10", 17.6);
    back.utc_outages = vec![(FIRST_WINDOW_W - 600, FIRST_WINDOW_W + 1_200)];
    let mut skipped = daily_scenario("daily: UTC lost 01:55-03:00", 17.6);
    skipped.utc_outages = vec![(FIRST_WINDOW_W - 600, FIRST_WINDOW_W + 7_200)];
    let scenarios = [back, skipped];
    let results = run_all(&scenarios);

    let (sc, r) = (&scenarios[0], &results[0]);
    check(sc, r);
    let tods = announce_tods_s(r);
    println!(
        "[{}] corrections {:?} at {tods:?} s",
        sc.label, r.corrections
    );
    assert_eq!(r.corrections.len(), 2, "{:?}", r.corrections);
    // Review round 1: once UTC is back (02:10), the step waits for the level to hold
    // MICRO_MIN_READINGS fresh readings (the sixth arrives 50 s later) — never one reading alone.
    assert!(
        (7_850.0..7_861.0).contains(&tods[0]),
        "the first night steps once UTC is back and read six times: {tods:?}"
    );
    assert_in_window(sc.label, &tods[1..], 7_200.0);
    // The estimate after the gap still knows the whole error (stepped rounded to 200 ms).
    let first = accrued(
        17.6,
        FIRST_WINDOW_W as f64 / 2.0 + (tods[0] - 7_200.0) + LANDING_S,
    );
    assert_rounded_step(
        sc.label,
        "the step after the gap",
        r.corrections[0].1,
        first,
    );

    let (sc, r) = (&scenarios[1], &results[1]);
    check(sc, r);
    let tods = announce_tods_s(r);
    println!(
        "[{}] corrections {:?} at {tods:?} s",
        sc.label, r.corrections
    );
    assert_eq!(
        r.corrections.len(),
        1,
        "one night skipped: {:?}",
        r.corrections
    );
    assert!(r.corrections[0].0.abs_diff(SECOND_WINDOW_W) <= 4);
    assert_in_window(sc.label, &tods, 7_200.0);
    let both = accrued(17.6, SECOND_WINDOW_W as f64 / 2.0 + LANDING_S);
    assert_rounded_step(sc.label, "both days' step", r.corrections[0].1, both);
    assert!(r.max_relative_phase_ns <= 50 * US);
    assert_eq!(r.wall_went_back, 0);
}

#[test]
fn a_master_only_ptp_outage_in_daily_mode_keeps_the_master_near_the_fleet_line_119() {
    // ONLY the master loses PTP for 30 minutes in the afternoon, ~0.1 s off UTC by then. In daily
    // mode it does not step its own wall to UTC (the fleet line is deliberately off UTC): it
    // free-runs on its learned frequency and re-joins the fleet line with one step of its measured
    // free-run error (≈ 0.3 ms here).
    let mut sc = daily_scenario("daily: master-only PTP outage", 17.6);
    sc.run_windows = 24 * 3_600 * 2;
    sc.master_ptp_offline = vec![(2 * 3_600 * 2, 2 * 3_600 * 2 + 3_600)];
    let r = run(&sc);
    check(&sc, &r);
    let joins: Vec<&Step> = r.steps[0]
        .iter()
        .filter(|s| s.2 == StepKind::Join)
        .collect();
    println!("[master outage] the master's re-joins: {joins:?}");
    assert_eq!(joins.len(), 1, "one re-join once PTP is back: {joins:?}");
    assert_eq!(r.wall_went_back, 0);
    assert!(
        joins.iter().all(|s| s.1.abs() < MS),
        "the master stayed near the fleet line: {joins:?}"
    );
    assert_eq!(r.corrections.len(), 1, "{:?}", r.corrections);
}

#[test]
fn a_master_without_ptp_over_the_window_still_takes_the_nightly_step_with_the_fleet_119() {
    // Review round 1: ONLY the master loses PTP from 01:40 to 02:20, across the window (and ending
    // before the grandmaster change at ~02:23, which would be the documented double fault). It still
    // announces the nightly step, and it takes it on its own wall with the fleet (its D is the
    // fleet D: in daily mode it never steps to UTC on its own) — otherwise a long outage would
    // leave it a day's drift off the fleet, and re-joining would be one large daytime step.
    let mut sc = daily_scenario("daily: master-only PTP outage over the window", 17.6);
    sc.run_windows = 24 * 3_600 * 2;
    sc.master_ptp_offline = vec![(FIRST_WINDOW_W - 2_400, FIRST_WINDOW_W + 2_400)];
    let r = run(&sc);
    check(&sc, &r);
    println!(
        "[master outage over the window] corrections {:?}; the master's steps {:?}",
        r.corrections, r.steps[0]
    );
    assert_eq!(r.corrections.len(), 1, "{:?}", r.corrections);
    let seq = r.announced[0].0;
    let own: Vec<&Step> = r.steps[0]
        .iter()
        .filter(|s| s.2 == StepKind::Coordinated && s.0 == seq)
        .collect();
    assert_eq!(own.len(), 1, "the master took the nightly step itself");
    assert_eq!(own[0].1, r.announced[0].1, "the whole announced step");
    let joins: Vec<&Step> = r.steps[0]
        .iter()
        .filter(|s| s.2 == StepKind::Join)
        .collect();
    assert!(
        joins.iter().all(|s| s.1.abs() < MS),
        "it re-joined the fleet line with its free-run error only: {joins:?}"
    );
    // Review round 2: its off-line landing is off the fleet's by its free-run error only (the
    // re-join measures that error), never by more.
    let mut fleet: Vec<f64> = r.steps[1..]
        .iter()
        .flatten()
        .filter(|s| s.2 == StepKind::Coordinated && s.0 == seq)
        .map(|s| s.3)
        .collect();
    fleet.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = fleet[fleet.len() / 2];
    let off = (own[0].3 - median).abs();
    let free_run = joins.iter().map(|s| s.1.abs()).max().unwrap_or(0) as f64;
    println!(
        "[master outage over the window] landing {off:.0} ns off the fleet, re-join {free_run} ns"
    );
    assert!(
        off <= free_run + 50_000.0,
        "the master landed {off} ns off the fleet, more than its free-run error {free_run} ns"
    );
    assert_eq!(r.wall_went_back, 0);
}
