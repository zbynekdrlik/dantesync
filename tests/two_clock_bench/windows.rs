//! dantesync#119 (1.11.1) — the bench's WINDOWS boxes: the production step law
//! (`dantesync::clock::step::step_wall`) against a model of the Windows clock, PTP samples stamped
//! by that stepped clock, and the scenario that proves a date micro-step every 20 s never moves the
//! rate servo.
//!
//! The model, from the rig (stream, 1.11.0, 26.9.2026):
//!
//! - the COARSE system time (`GetSystemTimeAsFileTime`) is the precise one at the last clock
//!   interrupt, a 0.5 ms tick (the raised timer resolution of a media PC);
//! - `NtSetSystemTime` sets the PRECISE time to its target a few µs after the call starts (rarely a
//!   preempted few hundred µs), then blocks ~117 ms (every `[StepClock]` line on stream);
//! - QPC runs at the wall's rate (the step law scales it by the adjustment) and never steps;
//! - Npcap stamps a packet with the precise system time at capture; the controller processes it at
//!   the end of its window, so a step can land between the stamp and the processing.

use super::*;
use dantesync::clock::step::{step_wall, ClockReading, StepLead, StepOps};

/// The Windows clock interrupt with a raised timer resolution.
const TICK_NS: i64 = 500 * US;
/// `NtSetSystemTime` blocks this long after it applied the time (measured on stream).
const SET_CALL_NS: i64 = 117 * MS;
/// Npcap's own queue on top of the window: a packet may be processed this long after the window
/// it was captured in ended.
const CAPTURE_QUEUE_NS: f64 = 20e6;
/// The Windows boxes of the scenario (box 0, the NTP master, stays ideal — the rig's master runs
/// Linux).
pub(super) const WINDOWS_BOXES: [usize; 3] = [1, 3, 5];

/// One box's Windows clock: its phase against the clock-interrupt grid, its own noise, the step
/// law's learned latency, and what its steps did.
pub(super) struct WinClock {
    lead: StepLead,
    rng: Rng,
    tick_phase_ns: i64,
    pub(super) max_residual_ns: i64,
}

impl WinClock {
    pub(super) fn new(i: usize, seed: u64) -> Self {
        let mut rng = Rng(0x5851_F42D_4C95_7F2D
            ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9)
            ^ seed.wrapping_mul(0xBF58_476D_1CE4_E5B9));
        let tick_phase_ns = (rng.uniform() * TICK_NS as f64) as i64;
        WinClock {
            lead: StepLead::default(),
            rng,
            tick_phase_ns,
            max_residual_ns: 0,
        }
    }

    /// The read→set latency of one `NtSetSystemTime` call: a few µs, 3 % preempted.
    fn set_latency_ns(&mut self) -> i64 {
        let base = 3_000.0 + self.rng.gauss().abs() * 2_000.0;
        let preempted = if self.rng.uniform() < 0.03 {
            150_000.0 + self.rng.uniform() * 250_000.0
        } else {
            0.0
        };
        (base + preempted).round() as i64
    }

    /// Step the wall by `requested_ns` at TRUE time `t_ns` through the production step law; returns
    /// what the wall actually moved.
    pub(super) fn step(&mut self, t_ns: f64, requested_ns: i64) -> i64 {
        let mut lead = self.lead;
        let mut os = WinOs {
            clock: self,
            // Only differences matter: the wall's value at the call is any epoch-scale number.
            wall0_ns: 1_790_000_000 * S,
            true_ns: t_ns.round() as i64,
            elapsed_ns: 0,
            stepped_ns: 0,
        };
        let out = step_wall(&mut os, &mut lead, requested_ns).expect("the model never refuses");
        let realized = os.stepped_ns;
        assert_eq!(
            out.realized_ns, realized,
            "the step law measured the wall's true move"
        );
        self.lead = lead;
        self.max_residual_ns = self.max_residual_ns.max((requested_ns - realized).abs());
        realized
    }

    /// How much earlier (in `t2 − t1`) a sample reads when it was stamped at its capture instant
    /// instead of at the window's end `t_end_ns`: the box's rate against the grandmaster over the
    /// queue age, and a step that landed after the capture (the stamp predates it).
    pub(super) fn capture_shift_ns(
        &mut self,
        t_end_ns: f64,
        rel_rate_ppm: f64,
        landing: Option<(f64, i64)>,
    ) -> i64 {
        let age = self.rng.uniform() * (TRUE_DT_NS + CAPTURE_QUEUE_NS);
        let captured = t_end_ns - age;
        let mut shift = (age * rel_rate_ppm * 1e-6).round() as i64;
        if let Some((at, realized)) = landing {
            if at > captured && at <= t_end_ns {
                shift += realized;
            }
        }
        shift
    }
}

/// The Windows clock during one step call.
struct WinOs<'a> {
    clock: &'a mut WinClock,
    wall0_ns: i64,
    true_ns: i64,
    elapsed_ns: i64,
    stepped_ns: i64,
}

impl StepOps for WinOs<'_> {
    fn read(&mut self) -> ClockReading {
        self.elapsed_ns += 300; // three back-to-back API calls
        let now_true = self.true_ns + self.elapsed_ns;
        let precise = self.wall0_ns + self.elapsed_ns + self.stepped_ns;
        let since_interrupt = (now_true + self.clock.tick_phase_ns).rem_euclid(TICK_NS);
        ClockReading {
            coarse_ns: precise - since_interrupt,
            precise_ns: precise,
            // QPC at the wall's rate: over the ~0.1 s call the frequency word (≤ 60 ppm) is < 7 µs
            // either way, and the law scales it out on the real box.
            reference_ns: self.elapsed_ns,
        }
    }

    fn set(&mut self, target_ns: i64) -> Result<(), String> {
        self.elapsed_ns += self.clock.set_latency_ns();
        self.stepped_ns = target_ns - (self.wall0_ns + self.elapsed_ns);
        self.elapsed_ns += SET_CALL_NS;
        Ok(())
    }
}

/// The rate audit of one box after the settle: the learned rate (the phase lock's integrator) and
/// the word's 20 s mean against the truth (the heard grandmaster's rate minus the oscillator's).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RateAudit {
    pub(super) max_integrator_err_ppm: f64,
    pub(super) max_mean_word_err_ppm: f64,
    block_sum_ppm: f64,
    block_windows: u32,
}

/// 20 s of 0.5 s windows: one micro-correction interval.
const AUDIT_BLOCK_WINDOWS: u32 = 40;

impl Box_ {
    /// After this window's word is known: audit it against the truth (only once `judged`).
    pub(super) fn audit_rate(&mut self, gm_ppm: f64, judged: bool) {
        if !judged {
            return;
        }
        let truth = gm_ppm - self.osc_ppm - self.wander_now_ppm;
        let a = &mut self.rate_audit;
        a.max_integrator_err_ppm = a
            .max_integrator_err_ppm
            .max((self.core.integrator_ppm() - truth).abs());
        a.block_sum_ppm += self.word_ppm - truth;
        a.block_windows += 1;
        if a.block_windows == AUDIT_BLOCK_WINDOWS {
            let mean = a.block_sum_ppm / AUDIT_BLOCK_WINDOWS as f64;
            a.max_mean_word_err_ppm = a.max_mean_word_err_ppm.max(mean.abs());
            a.block_sum_ppm = 0.0;
            a.block_windows = 0;
        }
    }
}

#[test]
fn windows_boxes_take_a_micro_step_every_20_s_without_moving_the_rate_119() {
    // The rig since 1.11.0: +500 µs every 20 s (the fleet date falls behind UTC faster than the
    // micro capacity, as at +30 ppm here). Windows followers beside a Linux master, 80 minutes, the
    // last hour judged. Acceptance (the #119 rate-leak slice): every box's learned rate within
    // ±0.5 ppm of the truth, the word's 20 s mean too, the fleet's relative phase within 50 µs.
    let scenarios: Vec<Scenario> = [0u64, 1, 2]
        .iter()
        .map(|&seed| {
            let mut sc = Scenario::plain("micro every 20 s on Windows boxes", 30.0, true);
            sc.windows_boxes = &WINDOWS_BOXES;
            sc.run_windows = 80 * 60 * 2;
            sc.settle_windows = 20 * 60 * 2;
            sc.seed = seed;
            sc
        })
        .collect();
    let results: Vec<RunResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = scenarios
            .iter()
            .map(|sc| scope.spawn(move || run(sc)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a bench run panicked"))
            .collect()
    });
    for (sc, r) in scenarios.iter().zip(&results) {
        let seed = sc.seed;
        let judged_from = sc.settle_windows as f64 * TRUE_DT_NS;
        let label = format!("[windows micro, seed {seed}]");
        for &i in &WINDOWS_BOXES {
            // (A step that lands SHORT leaves the box's PTP view of "now" behind the instant it
            // just applied, so it re-schedules that announce as a zero step — not counted.)
            let micro_steps = r.steps[i]
                .iter()
                .filter(|s| {
                    s.2 == StepKind::Coordinated
                        && s.3 > judged_from
                        && s.1 != 0
                        && s.1.abs() <= MICRO_STEP_NS
                })
                .count();
            println!(
                "{label} box {i}: {micro_steps} micro steps in the judged hour, worst step \
                 residual {} µs, learned rate off by ≤ {:.3} ppm, 20 s mean word off by ≤ {:.3} \
                 ppm",
                r.max_step_residual_ns[i] as f64 / 1e3,
                r.rate_audits[i].max_integrator_err_ppm,
                r.rate_audits[i].max_mean_word_err_ppm
            );
            assert!(
                micro_steps >= 170,
                "{label} box {i}: a micro-step every 20 s ({micro_steps} in the hour)"
            );
        }
        for (i, a) in r.rate_audits.iter().enumerate() {
            assert!(
                a.max_integrator_err_ppm <= 0.5,
                "{label} box {i}: the learned rate left the truth by {:.3} ppm",
                a.max_integrator_err_ppm
            );
            assert!(
                a.max_mean_word_err_ppm <= 0.5,
                "{label} box {i}: the 20 s mean word left the truth by {:.3} ppm",
                a.max_mean_word_err_ppm
            );
        }
        println!(
            "{label} relative phase max {} µs, max wall disagreement {} µs",
            r.max_relative_phase_ns / US,
            r.max_disagreement_ns / US
        );
        assert!(
            r.max_relative_phase_ns <= 50 * US,
            "{label} relative phase {} µs",
            r.max_relative_phase_ns / US
        );
        assert_eq!(r.wall_went_back, 0, "{label}");
        assert!(r.late_steps.iter().all(|&l| l == 0), "{label} late steps");
    }
}
