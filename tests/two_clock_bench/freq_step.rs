//! camera-box issue 1372 (dantesync slice) — a grandmaster FREQUENCY step, on two clocks.
//!
//! One box on the phase lock against one grandmaster, both integer-ns clocks from the bench's
//! world (`world.rs`): the box ticks at its oscillator (+23 ppm) plus the word, the grandmaster at
//! its own rate, and at some instant the grandmaster's rate STEPS (a Dante leader re-election under
//! the same identity: the video VLAN case, no re-anchor). Each 0.5 s window holds 4 PTP samples,
//! `wall_rx − gm_tx` with the box's path delay and the software-timestamp noise, and the core gets
//! their `raw[len/2]` median and `dt` in grandmaster time, as the controller does. The same world
//! (same seed) also drives the PLAIN PI — the law before the frequency-step follow, from the
//! module's public constants — so every "today" number is measured, not quoted.
//!
//! Proven (the seven cases of the design, comment 5894821454 on camera-box issue 1372):
//!
//! 1. A 25 ppm step (either sign, several noise seeds) is confirmed within 30 s; the LEARNED
//!    frequency (the integrator) is within 1 ppm of the new one from 30 s after the step on, and
//!    the phase error under 50 µs from 120 s on. The plain PI needs more than 5 minutes for the
//!    1 ppm (measured ~8 min). The APPLIED word also carries the pull that retires the phase the
//!    step left (≈ |e0|/τ ≈ 23 ppm right after the re-seed, decaying with τ): its 20 s mean is
//!    within 1 ppm of the grandmaster from ≤ 120 s on.
//! 2. Noise alone — 30 µs, and a heavy tail — confirms nothing over hours, and the words are the
//!    plain PI's, bit for bit.
//! 3. A single 500 µs delay spike, and a lasting path-delay change — also 60-120 µs changes under
//!    50 µs of sample noise, changes in two stages, and a small offset absorbed into `D` — are not
//!    steps. A path delay that RAMPS for longer than the ring can tell apart is the known limit:
//!    it may be re-seeded, and then it is reversed and bounded.
//! 4. A slow 0.1 ppm/min wander is not a step, and the words are the plain PI's, bit for bit.
//! 5. Back-to-back steps (+25, then −25 two minutes later) are both followed.
//! 6. `D` never moves and no window re-anchors: the date layer is not involved at all. A step that
//!    comes WITH an identity change (the audio VLAN: the new leader is another device) re-anchors
//!    once and is followed after it just the same.
//! 7. (The NTP-independence bench above still asserts bit-identical words across two UTC
//!    scenarios with the detector inside the core.)

use super::*;
use dantesync::ptp_phase_lock::{
    FreqStep, DT_MAX_S, DT_MIN_S, ERROR_CLAMP_US, FREQ_CLAMP_PPM, K_I_PER_S2, K_P_PER_S,
};

/// The box's oscillator vs true time, and the word the rate servo hands over (0.4 ppm off).
const BOX_OSC_PPM: f64 = 23.0;
const HANDOVER_WORD_PPM: f64 = -BOX_OSC_PPM + 0.4;
/// The box's one-way PTP path delay.
const PATH_DELAY_NS: f64 = 38_000.0;
/// The rig's Dante leader flip (29.9.2026).
const STEP_PPM: f64 = 25.0;
/// Locked for 10 minutes before anything happens.
const LOCKED_WINDOWS: u64 = 1_200;

/// What disturbs the PTP samples.
#[derive(Clone, Copy)]
struct Noise {
    /// Gaussian software-timestamp noise per sample (σ, ns).
    sigma_ns: f64,
    /// A heavy tail: this fraction of the samples is delayed by an exponential of this mean (ns).
    tail_fraction: f64,
    tail_mean_ns: f64,
}

const BENCH_NOISE: Noise = Noise {
    sigma_ns: PTP_NOISE_NS,
    tail_fraction: 0.0,
    tail_mean_ns: 0.0,
};

/// Scripted events of a world besides the rates and the path delay.
#[derive(Clone, Copy, Default)]
struct Events {
    /// The grandmaster CHANGES identity at this window (another device, another uptime): the box
    /// re-anchors `D` on it, as the controller's UUID-change path does (`request_rebase`).
    identity_change_at: Option<u64>,
    /// The date layer absorbs this offset (ns) into `D` at this window, with no wall step (a
    /// follower's join inside the absorb tolerance): a level shift of the phase error.
    absorb_at: Option<(u64, i64)>,
}

/// The law before the frequency-step follow: the same PI on the same error, with nothing else.
struct PlainPi {
    anchor_ns: Option<i64>,
    i_ppm: f64,
}

impl PlainPi {
    fn on_window(&mut self, median_ns: i64, handover_ppm: f64, dt_s: f64) -> f64 {
        // The first window anchors `D` and takes the rate servo's word over (bumpless).
        let anchor = *self.anchor_ns.get_or_insert(median_ns);
        if self.i_ppm.is_nan() {
            self.i_ppm = handover_ppm;
        }
        let dt = dt_s.clamp(DT_MIN_S, DT_MAX_S);
        let e_us = ((median_ns - anchor) as f64 / 1_000.0).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        self.i_ppm = (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        (self.i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
    }
}

enum Law {
    Core(PhaseLockCore),
    Plain(PlainPi),
}

impl Law {
    fn window(&mut self, median_ns: i64, dt_s: f64) -> (f64, Option<FreqStep>, AnchorEvent) {
        match self {
            Law::Core(c) => {
                let out = c.on_window(median_ns, true, HANDOVER_WORD_PPM, dt_s);
                (
                    out.freq_ppm.expect("locked from the start"),
                    out.freq_step,
                    out.event,
                )
            }
            Law::Plain(p) => (
                p.on_window(median_ns, HANDOVER_WORD_PPM, dt_s),
                None,
                AnchorEvent::None,
            ),
        }
    }
    fn integrator_ppm(&self) -> f64 {
        match self {
            Law::Core(c) => c.integrator_ppm(),
            Law::Plain(p) => p.i_ppm,
        }
    }
    fn anchor_ns(&self) -> Option<i64> {
        match self {
            Law::Core(c) => c.anchor_ns(),
            Law::Plain(p) => p.anchor_ns,
        }
    }
    fn request_rebase(&mut self) {
        match self {
            Law::Core(c) => c.request_rebase(),
            Law::Plain(p) => p.anchor_ns = None,
        }
    }
    fn absorb(&mut self, delta_ns: i64) {
        let anchor = self.anchor_ns().expect("anchored") + delta_ns;
        match self {
            Law::Core(c) => c.set_anchor(anchor),
            Law::Plain(p) => p.anchor_ns = Some(anchor),
        }
    }
}

/// One world, one law.
struct Run {
    /// Every window's word.
    words: Vec<f64>,
    /// Every window's integrator error vs the frequency it should hold (ppm).
    freq_err_ppm: Vec<f64>,
    /// Every window's APPLIED word vs the frequency it should hold (ppm): the rate the clock runs.
    word_err_ppm: Vec<f64>,
    /// Every window's TRUE phase error (µs): the box's `t2 − t1 − D` without the noise.
    phase_err_us: Vec<f64>,
    /// (window, the step) for every followed step.
    steps: Vec<(u64, FreqStep)>,
    /// Anchor events after the first window.
    anchor_events: u32,
    /// `D` at the first window and at the end.
    anchors: (Option<i64>, Option<i64>),
}

/// Run `windows` windows. `gm_ppm(w)` is the grandmaster's rate and `osc_ppm(w)` the box's
/// oscillator during window `w`; `delay_ns(w)` the path delay of window `w`'s samples.
#[allow(clippy::too_many_arguments)]
fn run_world(
    law: &mut Law,
    windows: u64,
    seed: u64,
    noise: Noise,
    gm_ppm: impl Fn(u64) -> f64,
    osc_ppm: impl Fn(u64) -> f64,
    delay_ns: impl Fn(u64) -> f64,
    events: Events,
) -> Run {
    let mut rng = Rng(0x5DEE_CE66_D1CE_4E5B ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut gm = Clock {
        ns: 5 * 86_400 * S,
        frac: 0.0,
    };
    let mut wall = Clock {
        ns: 1_790_000_000 * S,
        frac: 0.0,
    };
    let mut word = HANDOVER_WORD_PPM;
    let mut prev_gm = gm.ns;
    let mut out = Run {
        words: Vec::new(),
        freq_err_ppm: Vec::new(),
        word_err_ppm: Vec::new(),
        phase_err_us: Vec::new(),
        steps: Vec::new(),
        anchor_events: 0,
        anchors: (None, None),
    };
    for w in 0..windows {
        if events.identity_change_at == Some(w) {
            // Another device: three more days of uptime, and the box re-anchors on it.
            gm.ns += 3 * 86_400 * S;
            prev_gm = gm.ns;
            law.request_rebase();
        }
        if let Some((at, delta)) = events.absorb_at {
            if at == w {
                law.absorb(delta);
            }
        }
        gm.advance(TRUE_DT_NS, gm_ppm(w));
        wall.advance(TRUE_DT_NS, osc_ppm(w) + word);
        let delay = delay_ns(w);
        let mut samples: Vec<i64> = (0..SAMPLES_PER_WINDOW)
            .map(|_| {
                let mut n = rng.gauss() * noise.sigma_ns;
                if noise.tail_fraction > 0.0 && rng.uniform() < noise.tail_fraction {
                    n += -noise.tail_mean_ns * rng.uniform().max(1e-300).ln();
                }
                wall.ns - gm.ns + (delay + n).round() as i64
            })
            .collect();
        samples.sort();
        let median = samples[SAMPLES_PER_WINDOW / 2];
        // dt in grandmaster time, as the controller measures it (t1 to t1).
        let dt = (gm.ns - prev_gm) as f64 / 1e9;
        prev_gm = gm.ns;
        let (next_word, step, event) = law.window(median, dt);
        if w == 0 {
            out.anchors.0 = law.anchor_ns();
        } else if event != AnchorEvent::None {
            out.anchor_events += 1;
        }
        if let Some(s) = step {
            out.steps.push((w, s));
        }
        word = next_word;
        out.words.push(word);
        // The frequency the integrator should hold: the grandmaster's rate minus the oscillator's.
        let truth = gm_ppm(w + 1) - osc_ppm(w + 1);
        out.freq_err_ppm.push(law.integrator_ppm() - truth);
        out.word_err_ppm.push(word - truth);
        let anchor = law.anchor_ns().expect("anchored");
        out.phase_err_us
            .push((wall.ns - gm.ns + PATH_DELAY_NS as i64 - anchor) as f64 / 1_000.0);
    }
    out.anchors.1 = law.anchor_ns();
    out
}

fn core() -> Law {
    Law::Core(PhaseLockCore::new())
}

fn plain() -> Law {
    Law::Plain(PlainPi {
        anchor_ns: None,
        i_ppm: f64::NAN,
    })
}

/// Seconds after `from` until `series` stays within `bound` to the end (None: never).
fn settles(series: &[f64], from: u64, bound: f64) -> Option<f64> {
    let tail = &series[from as usize..];
    let last_out = tail.iter().rposition(|v| v.abs() >= bound);
    match last_out {
        None => Some(0.0),
        Some(i) if i + 1 < tail.len() => Some((i + 1) as f64 * WINDOW_S),
        Some(_) => None,
    }
}

/// The 20 s (40-window) moving mean of `series`, aligned to the window that ends each mean.
fn mean_20s(series: &[f64]) -> Vec<f64> {
    const N: usize = 40;
    let mut out = vec![f64::INFINITY; (N - 1).min(series.len())];
    out.extend(series.windows(N).map(|w| w.iter().sum::<f64>() / N as f64));
    out.truncate(series.len());
    out
}

fn step_at(at: u64, ppm: f64) -> impl Fn(u64) -> f64 {
    move |w| if w >= at { ppm } else { 0.0 }
}

#[test]
fn a_25_ppm_grandmaster_step_is_followed_in_seconds_not_minutes_1372() {
    // 15 minutes after the step.
    let after = 1_800;
    // (confirm s, learned-frequency settle s, phase settle s, applied-word 20 s-mean settle s,
    // the applied word's largest excursion from the re-seed window on, in ppm)
    let mut worst_core = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut plain_settle = f64::INFINITY;
    let mut plain_peak_us = 0.0f64;
    for seed in 0..6u64 {
        for sign in [1.0, -1.0] {
            let gm = step_at(LOCKED_WINDOWS, sign * STEP_PPM);
            let windows = LOCKED_WINDOWS + after;
            let r = run_world(
                &mut core(),
                windows,
                seed,
                BENCH_NOISE,
                &gm,
                |_| BOX_OSC_PPM,
                |_| PATH_DELAY_NS,
                Events::default(),
            );
            let label = format!("seed {seed}, step {:+}", sign * STEP_PPM);
            assert_eq!(r.steps.len(), 1, "{label}: exactly one step followed");
            let (w, fs) = r.steps[0];
            let confirm_s = (w + 1 - LOCKED_WINDOWS) as f64 * WINDOW_S;
            assert!(confirm_s <= 30.0, "{label}: confirmed after {confirm_s} s");
            assert!(
                (fs.step_ppm - sign * STEP_PPM).abs() < 1.5,
                "{label}: {fs:?}"
            );
            let f_settle = settles(&r.freq_err_ppm, LOCKED_WINDOWS, 1.0).expect("settles");
            let e_settle = settles(&r.phase_err_us, LOCKED_WINDOWS, 50.0).expect("settles");
            assert!(
                f_settle <= 30.0,
                "{label}: |f error| < 1 ppm only from {f_settle} s"
            );
            assert!(
                e_settle <= 120.0,
                "{label}: |phase error| < 50 us only from {e_settle} s"
            );
            // The RATE the clock runs: the word also carries the pull that retires the phase the
            // step left, so its 20 s mean — not the integrator — says when the box ticks with the
            // grandmaster again.
            let w_settle =
                settles(&mean_20s(&r.word_err_ppm), LOCKED_WINDOWS, 1.0).expect("settles");
            assert!(
                w_settle <= 120.0,
                "{label}: the applied word's 20 s mean within 1 ppm only from {w_settle} s"
            );
            let excursion = r.word_err_ppm[w as usize..]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()));
            // 6: D never moved, nothing re-anchored.
            assert_eq!(r.anchors.0, r.anchors.1, "{label}: D moved");
            assert_eq!(r.anchor_events, 0, "{label}");
            worst_core = (
                worst_core.0.max(confirm_s),
                worst_core.1.max(f_settle),
                worst_core.2.max(e_settle),
                worst_core.3.max(w_settle),
                worst_core.4.max(excursion),
            );

            let p = run_world(
                &mut plain(),
                windows,
                seed,
                BENCH_NOISE,
                &gm,
                |_| BOX_OSC_PPM,
                |_| PATH_DELAY_NS,
                Events::default(),
            );
            plain_settle =
                plain_settle.min(settles(&p.freq_err_ppm, LOCKED_WINDOWS, 1.0).unwrap_or(1e9));
            let peak = p.phase_err_us[LOCKED_WINDOWS as usize..]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()));
            plain_peak_us = plain_peak_us.max(peak);
        }
    }
    eprintln!(
        "25 ppm step: follow confirmed <= {:.1} s, learned frequency within 1 ppm from <= {:.1} s, \
         |e| < 50 us from <= {:.1} s, applied word (20 s mean) within 1 ppm from <= {:.1} s, \
         word excursion after the re-seed <= {:.1} ppm; the plain PI: learned frequency within \
         1 ppm from >= {:.1} s, phase peak {:.0} us",
        worst_core.0,
        worst_core.1,
        worst_core.2,
        worst_core.3,
        worst_core.4,
        plain_settle,
        plain_peak_us
    );
    assert!(
        plain_settle > 300.0,
        "the plain PI took only {plain_settle} s: the bench no longer shows the problem"
    );
    assert!(plain_peak_us > 500.0, "{plain_peak_us}");
    // The pull's rate is bounded by the phase the step left over τ (~450 µs / 20 s here).
    assert!(worst_core.4 <= 30.0, "word excursion {} ppm", worst_core.4);
}

#[test]
fn noise_alone_never_confirms_a_step_and_the_words_stay_the_plain_pi_1372() {
    let hours = 3u64;
    let noises = [
        Noise {
            sigma_ns: 30_000.0,
            tail_fraction: 0.0,
            tail_mean_ns: 0.0,
        },
        // A heavy tail: one sample in ten queued behind 100 µs on average.
        Noise {
            sigma_ns: 30_000.0,
            tail_fraction: 0.1,
            tail_mean_ns: 100_000.0,
        },
    ];
    for (i, noise) in noises.into_iter().enumerate() {
        for seed in [11u64, 12] {
            let windows = hours * 3_600 * 2;
            let args = |law: &mut Law| {
                run_world(
                    law,
                    windows,
                    seed,
                    noise,
                    |_| 0.0,
                    |_| BOX_OSC_PPM,
                    |_| PATH_DELAY_NS,
                    Events::default(),
                )
            };
            let r = args(&mut core());
            assert!(r.steps.is_empty(), "noise {i} seed {seed}: {:?}", r.steps);
            let p = args(&mut plain());
            assert!(
                r.words
                    .iter()
                    .zip(&p.words)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "noise {i} seed {seed}: the words left the plain PI"
            );
        }
    }
}

#[test]
fn a_delay_spike_or_a_path_delay_change_is_not_a_step_1372() {
    let at = LOCKED_WINDOWS;
    type Delay = Box<dyn Fn(u64) -> f64>;
    let cases: [(&str, Delay); 3] = [
        (
            "one window 500 us late",
            Box::new(move |w| PATH_DELAY_NS + if w == at { 500_000.0 } else { 0.0 }),
        ),
        (
            "the path 200 us longer from now on",
            Box::new(move |w| PATH_DELAY_NS + if w >= at { 200_000.0 } else { 0.0 }),
        ),
        (
            "the path 300 us shorter from now on",
            Box::new(move |w| PATH_DELAY_NS - if w >= at { 300_000.0 } else { 0.0 }),
        ),
    ];
    for (label, delay) in &cases {
        for seed in 0..3u64 {
            let r = run_world(
                &mut core(),
                LOCKED_WINDOWS + 1_200,
                seed,
                BENCH_NOISE,
                |_| 0.0,
                |_| BOX_OSC_PPM,
                delay,
                Events::default(),
            );
            assert!(r.steps.is_empty(), "{label}, seed {seed}: {:?}", r.steps);
        }
    }
}

#[test]
fn a_slow_wander_is_not_a_step_and_the_words_stay_the_plain_pi_1372() {
    // 0.1 ppm per minute for two hours (12 ppm in all).
    let osc = |w: u64| BOX_OSC_PPM + 0.1 * (w as f64 * WINDOW_S) / 60.0;
    let windows = 2 * 3_600 * 2;
    let r = run_world(
        &mut core(),
        windows,
        3,
        BENCH_NOISE,
        |_| 0.0,
        osc,
        |_| PATH_DELAY_NS,
        Events::default(),
    );
    assert!(r.steps.is_empty(), "{:?}", r.steps);
    let p = run_world(
        &mut plain(),
        windows,
        3,
        BENCH_NOISE,
        |_| 0.0,
        osc,
        |_| PATH_DELAY_NS,
        Events::default(),
    );
    assert!(r
        .words
        .iter()
        .zip(&p.words)
        .all(|(a, b)| a.to_bits() == b.to_bits()));
    // The loop still tracks the ramp: a ramp r leaves a steady phase error of r / K_I (~17 µs).
    let late = &r.phase_err_us[r.phase_err_us.len() / 2..];
    assert!(late.iter().all(|e| e.abs() < 100.0));
}

#[test]
fn back_to_back_steps_are_both_followed_1372() {
    let second = LOCKED_WINDOWS + 240; // two minutes later
    let gm = move |w: u64| {
        if w >= second {
            0.0
        } else if w >= LOCKED_WINDOWS {
            STEP_PPM
        } else {
            0.0
        }
    };
    for seed in 0..4u64 {
        let r = run_world(
            &mut core(),
            second + 1_200,
            seed,
            BENCH_NOISE,
            gm,
            |_| BOX_OSC_PPM,
            |_| PATH_DELAY_NS,
            Events::default(),
        );
        assert_eq!(r.steps.len(), 2, "seed {seed}: {:?}", r.steps);
        let (w1, s1) = r.steps[0];
        let (w2, s2) = r.steps[1];
        assert!(w1 < second && (w1 + 1 - LOCKED_WINDOWS) as f64 * WINDOW_S <= 30.0);
        assert!(w2 >= second && (w2 + 1 - second) as f64 * WINDOW_S <= 30.0);
        assert!((s1.step_ppm - STEP_PPM).abs() < 1.5, "{s1:?}");
        assert!((s2.step_ppm + STEP_PPM).abs() < 1.5, "{s2:?}");
        let f_settle = settles(&r.freq_err_ppm, second, 1.0).expect("settles");
        assert!(f_settle <= 30.0, "seed {seed}: {f_settle} s");
        assert_eq!(r.anchors.0, r.anchors.1);
        assert_eq!(r.anchor_events, 0);
    }
}

#[test]
fn a_path_change_under_heavy_noise_or_an_absorb_is_not_a_step_1372() {
    // 50 µs of sample noise: the F test alone lacks the power to see a 60-120 µs path change,
    // whose level shift then reads as a 5-9 ppm slope. The shifted slope (the most likely level
    // shift explained away) keeps it from being re-seeded.
    let heavy = Noise {
        sigma_ns: 50_000.0,
        tail_fraction: 0.0,
        tail_mean_ns: 0.0,
    };
    let at = LOCKED_WINDOWS;
    for shift_us in [60.0, 80.0, 100.0, 120.0, -80.0] {
        for seed in 0..6u64 {
            let r = run_world(
                &mut core(),
                LOCKED_WINDOWS + 600,
                100 + seed,
                heavy,
                |_| 0.0,
                |_| BOX_OSC_PPM,
                move |w| PATH_DELAY_NS + if w >= at { shift_us * 1_000.0 } else { 0.0 },
                Events::default(),
            );
            assert!(
                r.steps.is_empty(),
                "a {shift_us} us path change, seed {seed}: {:?}",
                r.steps
            );
        }
    }
    // A follower's join inside the absorb tolerance moves D by up to 100 µs with no wall step:
    // the same level shift, from the date layer.
    for (noise, label) in [(BENCH_NOISE, "bench"), (heavy, "heavy")] {
        for delta_ns in [100_000i64, -100_000, 80_000] {
            for seed in 0..3u64 {
                let r = run_world(
                    &mut core(),
                    LOCKED_WINDOWS + 600,
                    200 + seed,
                    noise,
                    |_| 0.0,
                    |_| BOX_OSC_PPM,
                    |_| PATH_DELAY_NS,
                    Events {
                        absorb_at: Some((at, delta_ns)),
                        ..Events::default()
                    },
                );
                assert!(
                    r.steps.is_empty(),
                    "{label} noise, absorb {delta_ns} ns, seed {seed}: {:?}",
                    r.steps
                );
            }
        }
    }
}

#[test]
fn a_path_change_in_two_stages_is_not_a_step_1372() {
    // Review round 2: two path changes a few seconds apart — the second one sits inside the ring
    // with the first, so neither the F test against the residual after one split nor the slope
    // with ONE shift explained saw through it (a false re-seed in 13-23 of 25 seeded replica runs
    // at 20-30 µs). The white-noise F and the two-shift slope do.
    let at = LOCKED_WINDOWS;
    let cases = [
        (20_000.0, 75.0, 75.0, 14),
        (20_000.0, 100.0, 100.0, 20),
        (20_000.0, 80.0, 90.0, 16),
        (30_000.0, 90.0, 90.0, 18),
        (30_000.0, 70.0, 70.0, 12),
    ];
    for (sigma_ns, first_us, second_us, gap) in cases {
        let noise = Noise {
            sigma_ns,
            tail_fraction: 0.0,
            tail_mean_ns: 0.0,
        };
        for seed in 0..6u64 {
            let r = run_world(
                &mut core(),
                LOCKED_WINDOWS + 600,
                300 + seed,
                noise,
                |_| 0.0,
                |_| BOX_OSC_PPM,
                move |w| {
                    let first = if w >= at { first_us } else { 0.0 };
                    let second = if w >= at + gap { second_us } else { 0.0 };
                    PATH_DELAY_NS + (first + second) * 1_000.0
                },
                Events::default(),
            );
            assert!(
                r.steps.is_empty(),
                "{first_us} + {second_us} us {gap} windows apart at {} us noise, seed {seed}: {:?}",
                sigma_ns / 1_000.0,
                r.steps
            );
        }
    }
}

#[test]
fn a_path_ramp_the_ring_cannot_tell_from_a_step_is_bounded_and_reversed_1372() {
    // The known limit (review round 3): a path delay that ramps 300 µs over 20 s reads, inside a
    // 20 s ring, exactly like a 15 ppm frequency change. It may be re-seeded; what must hold is that
    // it is reversed once the ramp ends (the ring then shows the frequency the loop misses), that
    // nothing chases itself (at most the event and its reversal), and that the learned frequency
    // is back on the grandmaster's within minutes. Pinned as bounds, so removing the false event
    // later keeps the test green.
    let at = LOCKED_WINDOWS;
    let ramp_windows = 40u64;
    let mut worst = (0usize, 0.0f64, 0.0f64); // (events, learned-frequency settle s, peak |e| µs)
    for seed in 0..6u64 {
        let r = run_world(
            &mut core(),
            LOCKED_WINDOWS + 1_200,
            400 + seed,
            BENCH_NOISE,
            |_| 0.0,
            |_| BOX_OSC_PPM,
            move |w| {
                let into = w.saturating_sub(at).min(ramp_windows) as f64 / ramp_windows as f64;
                PATH_DELAY_NS + if w >= at { 300_000.0 * into } else { 0.0 }
            },
            Events::default(),
        );
        assert!(r.steps.len() <= 2, "seed {seed}: {:?}", r.steps);
        if let [(_, a), (_, b)] = r.steps[..] {
            assert!(
                a.step_ppm * b.step_ppm < 0.0,
                "a reversal, never a chase: {a:?} {b:?}"
            );
        }
        let f_settle = settles(&r.freq_err_ppm, at, 1.0).expect("the learned frequency returns");
        assert!(f_settle <= 240.0, "seed {seed}: {f_settle} s");
        let peak = r.phase_err_us[at as usize..]
            .iter()
            .fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak < 1_000.0, "seed {seed}: {peak} us");
        worst = (
            worst.0.max(r.steps.len()),
            worst.1.max(f_settle),
            worst.2.max(peak),
        );
    }
    eprintln!(
        "300 us path ramp over 20 s: <= {} events, learned frequency back within 1 ppm from <= \
         {:.1} s, true phase peak <= {:.0} us",
        worst.0, worst.1, worst.2
    );
}

#[test]
fn a_step_that_comes_with_a_new_grandmaster_is_followed_after_the_re_anchor_1372() {
    // The audio VLAN: the new leader is another device (another identity, uptime and oscillator,
    // 25 ppm off). The box re-anchors `D` on it at once, then follows its frequency.
    for seed in 0..4u64 {
        for sign in [1.0, -1.0] {
            let r = run_world(
                &mut core(),
                LOCKED_WINDOWS + 1_200,
                seed,
                BENCH_NOISE,
                step_at(LOCKED_WINDOWS, sign * STEP_PPM),
                |_| BOX_OSC_PPM,
                |_| PATH_DELAY_NS,
                Events {
                    identity_change_at: Some(LOCKED_WINDOWS),
                    ..Events::default()
                },
            );
            let label = format!("seed {seed}, step {:+}", sign * STEP_PPM);
            assert_eq!(
                r.anchor_events, 1,
                "{label}: one re-anchor, on the new grandmaster"
            );
            assert_eq!(r.steps.len(), 1, "{label}: {:?}", r.steps);
            let (w, fs) = r.steps[0];
            assert!(
                (w + 1 - LOCKED_WINDOWS) as f64 * WINDOW_S <= 30.0,
                "{label}: {w}"
            );
            assert!(
                (fs.step_ppm - sign * STEP_PPM).abs() < 1.5,
                "{label}: {fs:?}"
            );
            let f_settle = settles(&r.freq_err_ppm, LOCKED_WINDOWS, 1.0).expect("settles");
            let e_settle = settles(&r.phase_err_us, LOCKED_WINDOWS, 50.0).expect("settles");
            assert!(f_settle <= 30.0, "{label}: {f_settle} s");
            assert!(e_settle <= 120.0, "{label}: {e_settle} s");
        }
    }
}

/// 7, with the detector FIRING: the whole fleet bench (six boxes, the master's authority, the
/// followers, the micro-corrections) against grandmaster A whose rate steps by −25 ppm under the
/// same identity. Two runs that differ ONLY in UTC (+8 / +20 ppm vs the grandmaster; the fleet
/// slows down, so it falls further behind UTC and every correction stays a forward step, no slew
/// term in any word) give every box bit-identical frequency words: the follow reads no NTP. And
/// every box follows the flip once, within 30 s, with no re-anchor.
#[test]
fn the_fleet_follows_a_grandmaster_flip_and_the_words_stay_ntp_independent_1372() {
    let flip_at = 2 * 3_600 * 2;
    let make = |label, utc_vs_gm_ppm| Scenario {
        gm_a_flip: Some((flip_at, -STEP_PPM)),
        run_windows: 4 * 3_600 * 2,
        ..Scenario::plain(label, utc_vs_gm_ppm, false)
    };
    let (sc_a, sc_b) = (
        make("GM A flips -25 ppm, UTC +8 ppm", 8.0),
        make("GM A flips -25 ppm, UTC +20 ppm", 20.0),
    );
    let (a, b) = (run(&sc_a), run(&sc_b));
    for r in [&a, &b] {
        // The fleet's phase and rate envelopes hold through the flip: the walls within 100 µs at
        // every instant, every box's rate the grandmaster's in every clean hour. (The bench's full
        // `check()` is for its 24 h runs: it also expects the grandmaster change and reboot, and a
        // UTC drift inside the micro capacity, which 25 ppm on top of the rig's drift is not.)
        assert!(
            r.max_disagreement_ns < 100 * US,
            "{} µs",
            r.max_disagreement_ns / US
        );
        let worst = r.rate_errors_ppm.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        assert!(
            worst < 0.01,
            "an hourly rate left the grandmaster's by {worst} ppm"
        );
        assert!(r.late_steps.iter().all(|&l| l == 0), "{:?}", r.late_steps);
    }
    for i in 0..a.words.len() {
        assert_eq!(
            a.words[i], b.words[i],
            "box {i}: the frequency word depends on UTC through the step follow"
        );
        assert_eq!(a.freq_steps[i].len(), 1, "box {i}: {:?}", a.freq_steps[i]);
        let (w, step) = a.freq_steps[i][0];
        assert!(
            w >= flip_at && (w + 1 - flip_at) as f64 * WINDOW_S <= 30.0,
            "box {i}: followed at window {w}"
        );
        assert!((step + STEP_PPM).abs() < 1.5, "box {i}: {step}");
    }
    assert_eq!(a.freq_steps, b.freq_steps);
    assert_eq!(a.rebases, b.rebases, "no re-anchor for a flip");
    // The date genuinely differed between the runs, and only ever stepped forward.
    assert_ne!(a.announced.len(), b.announced.len());
    assert!(a.announced_slews.is_empty() && b.announced_slews.is_empty());
    assert!(a.announced.iter().chain(&b.announced).all(|s| s.1 > 0));
    eprintln!(
        "fleet flip: followed at windows {:?} (flip at {flip_at}); max wall disagreement {} µs \
         / {} µs",
        a.freq_steps.iter().map(|s| s[0].0).collect::<Vec<_>>(),
        a.max_disagreement_ns / US,
        b.max_disagreement_ns / US
    );
}
