//! dantesync#119 follow-up — the micro-correction decision, driven CLOSED-LOOP: a simulated fleet
//! line whose UTC error drifts, is read every 10 s with noise, and moves by exactly the increments
//! the scheduler decides (at their instant: a step 10 s after the decision, a slew from there at
//! 100 ppm). A constant input would never exercise the estimate's memory (see the rule
//! "A mock that returns a CONSTANT cannot test a control loop").

use super::*;

const US: i64 = 1_000;
const MS: i64 = 1_000_000;
const S: i64 = 1_000_000_000;
/// The authority announces a micro-correction two 5 s leads ahead (`MICRO_LEAD_FACTOR`).
const LEAD: i64 = 10 * S;

/// xorshift64* — deterministic, seeded (never an unseeded RNG in a statistic, rule #117 round 5).
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// The bench's WAN reading noise: σ = 400 µs.
fn gauss_400us(r: &mut Rng) -> i64 {
    (r.gauss() * 400_000.0).round() as i64
}

/// ±5 ms ASYMMETRIC jitter (a mobile-data UTC path): half the readings are delayed by up to +5 ms
/// one way, the other half early by up to 1.5 ms, on top of the 400 µs base noise.
fn asymmetric_5ms(r: &mut Rng) -> i64 {
    let j = if r.uniform() < 0.5 {
        r.uniform() * 5_000_000.0
    } else {
        -r.uniform() * 1_500_000.0
    };
    (j + r.gauss() * 400_000.0).round() as i64
}

struct World {
    /// Worst |UTC − fleet line| after the first 10 minutes (ns).
    max_error_ns: i64,
    increments: Vec<i64>,
    /// Consecutive increments of opposite sign.
    reversals: usize,
    behind_raised: bool,
    final_rate_ns_per_min: Option<f64>,
    final_error_ns: i64,
}

/// Run `hours` of a fleet line drifting at `drift_ppm` against UTC (plus `jumps`: (s, ns)).
fn run(
    drift_ppm: f64,
    noise: fn(&mut Rng) -> i64,
    hours: i64,
    seed: u64,
    jumps: &[(i64, i64)],
) -> World {
    let mut m = MicroScheduler::new(MicroConfig::default());
    let mut rng = Rng(seed);
    let dt = S / 2;
    let mut err = 0.0f64; // UTC − fleet line, ns
                          // Announced increments not yet (fully) landed: (start, end, amount, paid).
    let mut landing: Vec<(i64, i64, i64, i64)> = Vec::new();
    let mut w = World {
        max_error_ns: 0,
        increments: Vec::new(),
        reversals: 0,
        behind_raised: false,
        final_rate_ns_per_min: None,
        final_error_ns: 0,
    };
    let base = 7 * 86_400 * S; // PTP time: a grandmaster a week up
    let mut t = 0i64;
    while t < hours * 3600 * S {
        t += dt;
        let p = base + t;
        err += drift_ppm * 1e-6 * dt as f64;
        for &(at, j) in jumps {
            if at * S == t {
                err += j as f64;
            }
        }
        // Land what is due: a step at its instant, a slew at 100 ppm from its start.
        landing.retain_mut(|(start, end, amount, paid)| {
            if p < *start {
                return true;
            }
            let due = if *end == *start || p >= *end {
                *amount
            } else {
                (*amount as i128 * (p - *start) as i128 / (*end - *start) as i128) as i64
            };
            err -= (due - *paid) as f64;
            *paid = due;
            p < *end
        });
        let outstanding: i64 = landing.iter().map(|l| l.2 - l.3).sum();
        if t % (10 * S) == 0 {
            let reading = err.round() as i64 + noise(&mut rng);
            m.record(reading - outstanding, p);
            if m.update_falling_behind(p) == Some(true) {
                w.behind_raised = true;
            }
        }
        // One change at a time, like the authority: nothing new while one is landing.
        if landing.is_empty() {
            if let Some(inc) = m.decide(p, p + LEAD) {
                if let Some(&prev) = w.increments.last() {
                    if prev.signum() != inc.signum() {
                        w.reversals += 1;
                    }
                }
                w.increments.push(inc);
                let start = p + LEAD;
                let end = if inc > 0 {
                    start
                } else {
                    start + inc.abs() * 1_000_000 / 100
                };
                landing.push((start, end, inc, 0));
            }
        }
        if t > 600 * S
            && !jumps
                .iter()
                .any(|&(at, _)| (at * S..at * S + 3600 * S).contains(&t))
        {
            w.max_error_ns = w.max_error_ns.max(err.abs().round() as i64);
        }
    }
    w.final_rate_ns_per_min = m.correction_rate_ns_per_min(base + t);
    w.final_error_ns = err.round() as i64;
    w
}

#[test]
fn the_configured_step_and_interval_are_clamped_and_zero_means_the_default_119() {
    assert_eq!(clamp_micro_step_us(0), 500);
    assert_eq!(clamp_micro_step_us(10), 50);
    assert_eq!(clamp_micro_step_us(700), 700);
    assert_eq!(clamp_micro_step_us(50_000), 1_000);
    assert_eq!(clamp_micro_interval_s(0), 20);
    assert_eq!(clamp_micro_interval_s(1), 10);
    assert_eq!(clamp_micro_interval_s(45), 45);
    assert_eq!(clamp_micro_interval_s(100_000), 600);
    let c = MicroConfig::default();
    assert_eq!(c.step_ns, 500 * US);
    assert_eq!(c.interval_ns, 20 * S);
    assert_eq!(c.dead_band_ns, 2 * MS);
    // 500 µs per 20 s: the capacity is 1.5 ms/min, above the rig's measured 1.06 ms/min.
    assert_eq!(c.capacity_ns_per_min(), 1_500 * US);
}

#[test]
fn inside_the_dead_band_nothing_is_corrected_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    let p0 = 1_000 * S;
    for i in 0..30 {
        m.record(1_900 * US, p0 + i * 10 * S);
    }
    let now = p0 + 300 * S;
    assert_eq!(m.estimate(now).map(|e| e.error_ns), Some(1_900 * US));
    assert_eq!(m.decide(now, now + LEAD), None);
}

#[test]
fn no_estimate_and_no_correction_before_six_readings_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..5 {
        m.record(40 * MS, i * 10 * S);
    }
    assert_eq!(m.estimate(50 * S), None);
    assert_eq!(m.decide(50 * S, 55 * S), None);
    m.record(40 * MS, 50 * S);
    assert_eq!(m.decide(50 * S, 55 * S), Some(500 * US));
}

#[test]
fn beyond_the_dead_band_one_step_sized_increment_per_interval_in_the_errors_direction_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        m.record(-7 * MS, i * 10 * S);
    }
    let now = 100 * S;
    assert_eq!(m.decide(now, now + LEAD), Some(-500 * US));
    // Recorded at once: the kept readings now describe the error left after it.
    assert_eq!(m.estimate(now).map(|e| e.error_ns), Some(-6_500 * US));
    assert_eq!(m.last_increment_ns(), Some(-500 * US));
    // The next one waits for the interval (PTP time), then continues.
    assert_eq!(m.decide(now + 19 * S, now + 24 * S), None);
    assert_eq!(m.decide(now + 20 * S, now + 25 * S), Some(-500 * US));
    // Once correcting, the corrections continue past the dead band…
    let mut small = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        small.record(2_300 * US, i * 10 * S);
    }
    assert_eq!(small.decide(now, now + LEAD), Some(500 * US));
    assert_eq!(small.decide(now + 20 * S, now + 25 * S), Some(500 * US));
    assert_eq!(small.decide(now + 40 * S, now + 45 * S), Some(500 * US));
    assert_eq!(small.decide(now + 60 * S, now + 65 * S), Some(500 * US));
    // … down to the exit band (0.3 ms left), then it stops.
    assert_eq!(small.decide(now + 80 * S, now + 85 * S), None);
}

#[test]
fn the_estimate_projects_a_steady_ramp_without_lag_119() {
    // A +17.6 ppm ramp read every 10 s for 5 minutes: the estimate at the landing instant is the
    // ramp's value there (a plain median of the last readings would lag half its span).
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..=30 {
        let t = i * 10 * S;
        m.record((17.6e-6 * t as f64).round() as i64, t);
    }
    let land = 305 * S;
    let want = (17.6e-6 * land as f64).round() as i64;
    let est = m.estimate(land).unwrap();
    assert!((est.error_ns - want).abs() <= 2, "{est:?} vs {want}");
    assert!((est.trend_ns_per_s - 17_600.0).abs() < 0.01, "{est:?}");
}

#[test]
fn a_correction_against_the_recent_direction_needs_twice_the_dead_band_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        m.record(3 * MS, i * 10 * S);
    }
    let now = 100 * S;
    assert_eq!(m.decide(now, now + LEAD), Some(500 * US));
    // The readings now say −3 ms (e.g. an upstream jump): opposite to the last correction.
    m.compensate(5_500 * US); // 2.5 ms − 5.5 ms = −3 ms left
    assert_eq!(m.estimate(now + 20 * S).unwrap().error_ns, -3 * MS);
    assert_eq!(
        m.decide(now + 20 * S, now + 25 * S),
        None,
        "3 ms against the last direction is inside the 4 ms reversal band"
    );
    m.compensate(1_500 * US); // −4.5 ms
    assert_eq!(m.decide(now + 40 * S, now + 45 * S), Some(-500 * US));
}

#[test]
fn the_direction_turns_with_the_drift_itself_119() {
    // The last correction was forward; now the readings DRIFT backwards (the grandmaster's rate
    // changed): −2.5 ms on a −17.6 ppm ramp is corrected at the plain dead band.
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        m.record(3 * MS, i * 10 * S);
    }
    assert_eq!(m.decide(100 * S, 105 * S), Some(500 * US));
    // Twenty minutes of readings on the turned ramp, ending at −2.5 ms.
    let end = 1_310 * S;
    for i in 0..=120 {
        let t = 110 * S + i * 10 * S;
        m.record(-2_500 * US + (17.6e-6 * (end - t) as f64).round() as i64, t);
    }
    let now = end;
    let est = m.estimate(now + LEAD).unwrap();
    assert!(est.trend_ns_per_s < -MICRO_TURNED_TREND_NS_PER_S, "{est:?}");
    assert!(est.error_ns < -2_500 * US, "{est:?}");
    assert_eq!(m.decide(now, now + LEAD), Some(-500 * US));
}

#[test]
fn a_rebase_moves_the_kept_instants_and_the_estimate_with_the_time_base_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..=30 {
        let t = i * 10 * S;
        m.record((17.6e-6 * t as f64).round() as i64, t);
    }
    let before = m.estimate(310 * S).unwrap();
    let before_rate = m.correction_rate_ns_per_min(310 * S);
    let shift = 5 * 86_400 * S; // the new grandmaster's uptime is 5 days less
    m.rebase(shift);
    let after = m.estimate(310 * S - shift).unwrap();
    assert_eq!(before, after);
    assert_eq!(m.correction_rate_ns_per_min(310 * S - shift), before_rate);
}

#[test]
fn a_cleared_scheduler_needs_fresh_readings_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        m.record(9 * MS, i * 10 * S);
    }
    m.clear();
    assert_eq!(m.estimate(100 * S), None);
    assert_eq!(m.decide(100 * S, 105 * S), None);
}

#[test]
fn a_day_at_plus_17_6_ppm_holds_utc_within_3_ms_in_increments_of_at_most_500_us_119() {
    let w = run(17.6, gauss_400us, 24, 0x5EED_0001, &[]);
    println!(
        "+17.6 ppm: {} increments, max error {} µs, rate {:?} ns/min",
        w.increments.len(),
        w.max_error_ns / US,
        w.final_rate_ns_per_min
    );
    assert!(w.increments.iter().all(|&i| i > 0 && i <= 500 * US));
    assert!(
        w.max_error_ns <= 3 * MS,
        "max error {} µs",
        w.max_error_ns / US
    );
    assert_eq!(w.reversals, 0);
    assert!(
        !w.behind_raised,
        "1.06 ms/min is within the 1.5 ms/min capacity"
    );
    // The measured correction rate is the drift: 17.6 ppm = 1.056 ms/min.
    let rate = w.final_rate_ns_per_min.unwrap();
    assert!((rate - 1_056_000.0).abs() < 200_000.0, "rate {rate}");
}

#[test]
fn a_day_at_minus_15_ppm_holds_utc_within_3_ms_backwards_119() {
    let w = run(-15.0, gauss_400us, 24, 0x5EED_0002, &[]);
    assert!(w.increments.iter().all(|&i| (-500 * US..0).contains(&i)));
    assert!(
        w.max_error_ns <= 3 * MS,
        "max error {} µs",
        w.max_error_ns / US
    );
    assert_eq!(w.reversals, 0);
    assert!(!w.behind_raised);
}

#[test]
fn asymmetric_5_ms_utc_jitter_never_makes_the_corrections_oscillate_119() {
    // No drift at all, only the noisy upstream: the scheduler may absorb the jitter's bias once,
    // but it must never correct back and forth.
    for seed in [0xA5A5_0001u64, 0xA5A5_0002, 0xA5A5_0003, 0xA5A5_0004] {
        let w = run(0.0, asymmetric_5ms, 24, seed, &[]);
        println!(
            "seed {seed:#x}: {} increments {:?}, max error {} µs",
            w.increments.len(),
            w.increments,
            w.max_error_ns / US
        );
        assert_eq!(w.reversals, 0, "seed {seed:#x}: {:?}", w.increments);
        assert!(
            w.increments.len() <= 10,
            "seed {seed:#x}: a day of pure jitter moved the date {} times",
            w.increments.len()
        );
    }
    // … and with the rig's drift on top it still holds, one direction only.
    let w = run(17.6, asymmetric_5ms, 24, 0xA5A5_0005, &[]);
    assert_eq!(w.reversals, 0);
    assert!(w.increments.iter().all(|&i| i > 0 && i <= 500 * US));
}

#[test]
fn a_drift_beyond_the_capacity_raises_the_alarm_and_never_takes_a_large_correction_119() {
    // 30 ppm = 1.8 ms/min against a 1.5 ms/min capacity: the error grows ~0.3 ms/min.
    let w = run(30.0, gauss_400us, 1, 0x5EED_0003, &[]);
    assert!(w.behind_raised, "the falling-behind alarm was raised");
    assert!(w.increments.iter().all(|&i| i > 0 && i <= 500 * US));
    assert!(
        w.final_error_ns > 10 * MS,
        "the error really grew: {} µs",
        w.final_error_ns / US
    );
}

#[test]
fn the_alarm_clears_once_the_corrections_caught_up_119() {
    let mut m = MicroScheduler::new(MicroConfig::default());
    for i in 0..10 {
        m.record(12 * MS, i * 10 * S);
    }
    assert_eq!(m.update_falling_behind(90 * S), Some(true));
    assert!(m.falling_behind());
    assert_eq!(
        m.update_falling_behind(90 * S),
        None,
        "reported once per change"
    );
    m.compensate(10_500 * US); // 1.5 ms left: inside the dead band
    assert_eq!(m.update_falling_behind(100 * S), Some(false));
}

#[test]
fn an_upstream_jump_is_absorbed_and_corrected_without_oscillation_119() {
    // +8 ppm, then UTC steps back 5 ms, later forward 8 ms (an upstream server change).
    let w = run(
        8.0,
        gauss_400us,
        12,
        0x5EED_0004,
        &[(3 * 3600, -5 * MS), (8 * 3600, 8 * MS)],
    );
    assert!(w.increments.iter().all(|&i| i.abs() <= 500 * US));
    assert!(
        w.max_error_ns <= 3 * MS,
        "max error {} µs",
        w.max_error_ns / US
    );
    assert!(
        w.reversals <= 2,
        "one turn per jump at most: {}",
        w.reversals
    );
}
