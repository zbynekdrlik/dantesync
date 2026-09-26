//! dantesync#119 follow-up — the bench's MICRO-correction scenarios: the fleet date corrected in
//! increments of at most 500 µs, at most one per 20 s, beyond a 2 ms dead band.

use super::*;

/// Consecutive announced corrections of opposite sign.
fn reversals(corrections: &[(u64, i64)]) -> usize {
    corrections
        .windows(2)
        .filter(|p| p[0].1.signum() != p[1].1.signum())
        .count()
}

/// The largest step any box applied after its boot join (ns).
fn largest_non_join_step(r: &RunResult) -> i64 {
    r.steps
        .iter()
        .flatten()
        .filter(|s| s.2 != StepKind::Join)
        .map(|s| s.1.abs())
        .max()
        .unwrap_or(0)
}

/// A production-like day: the controller's post-step grace, the micro estimate settled (10 min).
fn day(label: &'static str, utc_vs_gm_ppm: f64) -> Scenario {
    let mut sc = Scenario::plain(label, utc_vs_gm_ppm, true);
    sc.settle_windows = 10 * 60 * 2;
    sc
}

#[test]
fn a_day_of_micro_corrections_holds_the_date_within_3_ms_in_steps_of_at_most_500_us_119() {
    // +17.6 ppm is the rig's grandmaster-vs-UTC drift since 25.9.2026 (1.06 ms/min: a 50 ms
    // step every ~47 min before); −15 ppm runs the other way (every correction backward).
    for (sc, forward) in [
        (day("micro: UTC +17.6 ppm vs GM", 17.6), true),
        (day("micro: UTC -15 ppm vs GM", -15.0), false),
    ] {
        let label = sc.label;
        let r = run(&sc);
        check(&sc, &r);
        let largest = r.corrections.iter().map(|c| c.1.abs()).max().unwrap_or(0);
        println!(
            "[{label}] {} corrections (largest {} µs, {} reversals), largest step {} µs, master \
             |UTC − wall| max {} µs, fleet max {} µs, relative phase max {} µs",
            r.corrections.len(),
            largest / US,
            reversals(&r.corrections),
            largest_non_join_step(&r) / US,
            r.max_utc_error_ns / US,
            r.max_fleet_utc_error_ns / US,
            r.max_relative_phase_ns / US
        );
        assert!(
            r.corrections.len() > 1_000,
            "[{label}] the date is corrected continuously: {}",
            r.corrections.len()
        );
        assert!(
            largest <= 500 * US,
            "[{label}] a correction of {largest} ns"
        );
        assert!(
            r.corrections.iter().all(|c| (c.1 > 0) == forward),
            "[{label}] every correction follows the drift"
        );
        assert_eq!(reversals(&r.corrections), 0, "[{label}]");
        // No large step: nothing but the boot joins exceeds one micro-correction on any box (plus,
        // at a grandmaster event, the re-anchor residual a step may carry: see `same_steps`).
        assert!(
            largest_non_join_step(&r) <= 500 * US + dantesync::date_offset::ABSORB_TOLERANCE_NS,
            "[{label}] a step of {} µs",
            largest_non_join_step(&r) / US
        );
        if !forward {
            assert!(r.announced.is_empty(), "[{label}] never a backward step");
        }
        // The date: within 3 ms of UTC, on the master and on the fleet line.
        assert!(
            r.max_utc_error_ns <= 3 * MS,
            "[{label}] master {} µs off UTC",
            r.max_utc_error_ns / US
        );
        assert!(
            r.max_fleet_utc_error_ns <= 3 * MS,
            "[{label}] fleet {} µs off UTC",
            r.max_fleet_utc_error_ns / US
        );
        // The phase: every box within 50 µs of the others (each wall plus its own path delay).
        assert!(
            r.max_relative_phase_ns <= 50 * US,
            "[{label}] relative phase {} µs",
            r.max_relative_phase_ns / US
        );
        assert_eq!(r.wall_went_back, 0, "[{label}]");
    }
}

#[test]
fn asymmetric_5_ms_utc_jitter_never_makes_the_fleet_date_oscillate_119() {
    // A mobile-data UTC path: half the readings up to +5 ms late, half up to 1.5 ms early. UTC runs
    // with grandmaster A (no drift) for the first 12 hours, then −3 ppm against grandmaster B. The
    // dead band, the noise margin and the median absorb the jitter: the date may be moved a few
    // times to take its bias while there is no drift, it follows B's drift one way, and it never
    // goes back and forth.
    let mut sc = day("micro: ±5 ms asymmetric UTC jitter", 0.0);
    sc.ntp_noise = NtpNoise::Asymmetric5ms;
    let r = run(&sc);
    check(&sc, &r);
    let before_b: Vec<&(u64, i64)> = r
        .corrections
        .iter()
        .filter(|c| c.0 < GM_CHANGE_AT_WINDOW)
        .collect();
    println!(
        "[jitter] {} corrections, {} with no drift: {:?}",
        r.corrections.len(),
        before_b.len(),
        before_b
    );
    assert_eq!(reversals(&r.corrections), 0, "{:?}", r.corrections);
    assert!(
        before_b.len() <= 6,
        "12 hours of pure jitter moved the date {} times",
        before_b.len()
    );
    assert!(r
        .corrections
        .iter()
        .filter(|c| c.0 > GM_CHANGE_AT_WINDOW)
        .all(|c| c.1 == -500 * US));
    // … and with the rig's drift on top it still corrects one way only.
    let mut drift = day("micro: ±5 ms asymmetric UTC jitter, +17.6 ppm", 17.6);
    drift.ntp_noise = NtpNoise::Asymmetric5ms;
    let r = run(&drift);
    check(&drift, &r);
    assert_eq!(reversals(&r.corrections), 0);
    assert!(r.corrections.iter().all(|c| c.1 > 0 && c.1 <= 500 * US));
}
