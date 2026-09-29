//! dantesync#117 — the PTP PHASE LOCK: rate AND phase from the Dante grandmaster, nothing else.
//!
//! # The owner contract (issue #117, ROZHODNUTÉ issuecomment-5836853146)
//!
//! RATE = the Dante PTP tick only; NTP = date stepping only. Every PC running dantesync must tick
//! exactly with the grandmaster, and all PCs must show the same wall time. Before #117 the PTP
//! servo was RATE-ONLY: it drove `d(offset)/dt` to zero and never looked at the offset itself
//! (`initial_epoch_offset_ns` was written once and never read, and NANO mode ignored rates below a
//! 0.1 µs/s deadband). Rate-only is not phase: two boxes at "the same rate" random-walk apart
//! (0.1 ppm uncorrected = 360 µs/h), so cross-box agreement was held by NTP — through #97's
//! `phase_slew`, which steered the RATE by up to ±5-19 ppm away from the Dante tick.
//!
//! # What this module does
//!
//! It holds the system clock on `wall = PTP_time + D`, where `D` is the fleet date offset
//! (`crate::date_offset`). The error it drives to zero is
//!
//! ```text
//! e = (t2 − t1) − D          t1 = grandmaster send time (PTP), t2 = local receive time (wall)
//! ```
//!
//! with a PI controller whose integrator IS the learned oscillator frequency error:
//!
//! ```text
//! f = −(K_P·e + I),   I ← I + K_I·e·dt        (f in ppm = µs/s, e in µs)
//! ```
//!
//! It is ONE loop on ONE measurement (the PTP offset) giving both rate and phase — not a second
//! loop beside the rate servo — so the #97 decoupling proof reduces to a fact visible in the
//! signature of [`PhaseLockCore::on_window`]: no argument carries NTP. The only NTP-derived input
//! anywhere is `D`, and `D` changes only by a STEP of the wall of exactly the same size at the same
//! instant (`note_step`), which leaves `e` untouched. The bench (`tests/two_clock_bench.rs`)
//! proves it by running two UTC scenarios and asserting the frequency command sequences are
//! bit-identical.
//!
//! # Lifecycle
//!
//! - Acquisition stays on the existing PTP RATE servo in `controller.rs` (also pure PTP). This
//!   core engages once the controller reports PTP lock, taking over the frequency word
//!   bumplessly (its integrator starts at the rate servo's word), and hands the learned frequency
//!   back if lock is lost.
//! - `D` is anchored at the first lock (from the node's own, NTP-coarse-aligned wall), replaced by
//!   the master's published `D` when the node follows the authority (`set_anchor`), shifted by
//!   every applied step (`note_step`), and RE-ANCHORED from the continuous wall when the
//!   grandmaster changes (`request_rebase`) or its time base jumps by more than
//!   [`DISCONTINUITY_NS`] (a grandmaster reboot keeps its UUID but restarts its uptime). A
//!   re-anchor never steps the wall: the new grandmaster's time base is simply adopted.
//! - The NANO 0.1 µs/s deadband has no equivalent here: the phase term corrects every residual,
//!   so nothing drifts freely.
//!
//! # A grandmaster FREQUENCY step (camera-box issue 1372, dantesync slice)
//!
//! A Dante leader re-election moves the grandmaster's frequency by up to ~25 ppm at once, and the
//! slow PI above would take ~8-12 minutes to follow it (with a phase error near 1 ms on the way).
//! [`freq_step::FreqStepDetector`] measures the frequency the loop has not learned, from the
//! open-loop phase over the last 20 s of windows, and confirms a clean step. The core then:
//!
//! - re-seeds the integrator by the measured error (bounded to [`FSTEP_MAX_PPM`] per event), so
//!   the word follows the step at once;
//! - retires the phase the step left along a decaying reference ([`FSTEP_PULL_TAU_S`]): the PI
//!   tracks `e − r` and the word carries the reference's own rate `−r/τ`, so the recovery neither
//!   disturbs the integrator nor overshoots;
//! - never steps the wall and never touches `D`.
//!
//! Without a confirmed step nothing changes: the words are bit-identical to the plain PI.
//!
//! Pure (explicit inputs, no I/O, no logging) so the controller and the bench run the same code.

pub mod freq_step;

pub use freq_step::{
    fit_line, fit_ring, split_test, FreqStepDetector, FreqStepEstimate, LineFit, RingFit,
    FSTEP_CONFIRM, FSTEP_HOLDOFF_S, FSTEP_LINEARITY_F_MAX, FSTEP_MAX_PPM, FSTEP_MIN_PPM,
    FSTEP_SIGMAS, FSTEP_WINDOW_S,
};

/// Proportional gain, ppm per µs of phase error (i.e. 1/s).
///
/// Chosen with the integrator for a critically-damped second-order loop
/// (`s² + K_P·s + K_I`, ζ = K_P / (2·√K_I) = 1) with a ~100 s time constant. That is slow on
/// purpose: the error is a median of software-timestamped PTP samples (tens of µs of noise), and
/// every µs of noise the proportional term passes becomes 0.02 ppm of frequency jitter. The loop
/// only has to follow oscillator wander (fractions of a ppm per minute); a frequency ramp `r`
/// (ppm/s) leaves a steady phase error of `r / K_I` — 0.1 ppm/min ⇒ 17 µs.
pub const K_P_PER_S: f64 = 0.02;

/// Integral gain, ppm per µs·s (1/s²). `K_P² / 4` = critical damping.
pub const K_I_PER_S2: f64 = K_P_PER_S * K_P_PER_S / 4.0;

/// The phase error fed to the controller is clamped to ±this (µs), so a burst of bad samples, or
/// a large error on re-engagement, can command at most `K_P·clamp` = 40 ppm of proportional slew
/// and cannot wind the integrator up.
pub const ERROR_CLAMP_US: f64 = 2_000.0;

/// Output clamp (ppm) — the same ±500 ppm envelope the rate servo uses.
pub const FREQ_CLAMP_PPM: f64 = 500.0;

/// A measured `t2 − t1` further than this from `D` is not a phase error but a jump of the
/// grandmaster's time base (a GM reboot restarts its uptime under the same UUID). Re-anchor.
pub const DISCONTINUITY_NS: i64 = 1_000_000_000;

/// On (re-)ENGAGEMENT, a phase error larger than this (the clock free-ran while the rate servo
/// held it, e.g. through a lock loss) is not slewed back — at the proportional clamp that would
/// keep the rate off the PTP tick for minutes. `D` is re-anchored on the current offset instead
/// (the wall stays where it is), and the date layer re-aligns it with ONE step: a follower's
/// next authority poll sees its `D` differ and joins; the master rebases the fleet offset.
pub const REANCHOR_ON_ENGAGE_NS: i64 = 1_000_000;

/// `dt` is clamped to this range (s): a first update or a long gap must not scale the integrator
/// step by an unbounded interval.
pub const DT_MIN_S: f64 = 0.01;
pub const DT_MAX_S: f64 = 5.0;

/// Time constant (s) of the reference along which the phase a frequency step left is retired.
/// Its rate `−r/τ` is at most `ERROR_CLAMP_US / τ` = 100 ppm, inside the output clamp; 400 µs
/// (a 25 ppm step detected after ~20 s) is under 50 µs ~40 s later.
pub const FSTEP_PULL_TAU_S: f64 = 20.0;

/// Below this (µs) the pull reference is dropped (its rate is then < 0.03 ppm).
pub const FSTEP_PULL_DONE_US: f64 = 0.5;

/// What happened to the anchor `D` in one window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnchorEvent {
    None,
    /// First anchor (the first PTP lock).
    Anchored {
        anchor_ns: i64,
    },
    /// Re-anchored from the continuous wall because the TIME BASE changed: a grandmaster change
    /// (`request_rebase`), or a > 1 s jump of the grandmaster's time (a reboot under the same
    /// UUID). The fleet date offset follows it into the new base (the authority shifts it by the
    /// same base shift), with no wall step anywhere.
    Rebased {
        old_ns: i64,
        new_ns: i64,
    },
    /// Re-anchored on (re-)engagement more than [`REANCHOR_ON_ENGAGE_NS`] off `D` in the SAME time
    /// base: this box's wall wandered (it free-ran through a lock loss, or took local NTP steps).
    /// The fleet date offset must NOT follow — this box re-aligns its own wall to it with one step.
    Realigned {
        old_ns: i64,
        new_ns: i64,
    },
}

/// A grandmaster frequency step the loop followed (camera-box issue 1372).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreqStep {
    /// The step (ppm): the grandmaster's frequency relative to this box moved by this, so the word
    /// follows it (positive = the grandmaster sped up, the word goes up). Minus the measured error,
    /// bounded to ±[`FSTEP_MAX_PPM`].
    pub step_ppm: f64,
    /// The frequency the loop had not learned, as measured (ppm): the unbounded `−step_ppm`.
    pub error_ppm: f64,
    /// The slope's standard error (ppm) and the linearity statistic of the confirming fit.
    pub sigma_ppm: f64,
    pub linearity_f: f64,
    /// Seconds of windows the confirming fit spanned.
    pub span_s: f64,
    pub integrator_before_ppm: f64,
    pub integrator_after_ppm: f64,
    /// The phase error the step left (µs), retired along the pull reference.
    pub pull_us: f64,
}

/// The result of one PTP sample window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowOutcome {
    /// The frequency word to apply (ppm) while engaged; `None` = not engaged, the rate servo's
    /// word applies.
    pub freq_ppm: Option<f64>,
    /// `e = (t2 − t1) − D` (ns) once anchored.
    pub error_ns: Option<i64>,
    pub event: AnchorEvent,
    /// A frequency step confirmed and followed in THIS window (the controller logs it).
    pub freq_step: Option<FreqStep>,
}

/// The phase-lock state of one box.
#[derive(Clone, Debug, Default)]
pub struct PhaseLockCore {
    anchor_ns: Option<i64>,
    rebase_pending: bool,
    engaged: bool,
    /// Integrator = the learned frequency word (ppm), i.e. minus the oscillator's error vs the GM.
    i_ppm: f64,
    last_error_ns: Option<i64>,
    last_freq_ppm: f64,
    /// camera-box issue 1372: the frequency-step detector (reads only `e` and the words).
    fstep: FreqStepDetector,
    /// The phase (µs) still being retired after a followed frequency step; 0 = none.
    pull_us: f64,
    freq_steps: u32,
    last_freq_step: Option<FreqStep>,
}

impl PhaseLockCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// `D` (ns), once anchored.
    pub fn anchor_ns(&self) -> Option<i64> {
        self.anchor_ns
    }

    /// True while this core owns the frequency word.
    pub fn engaged(&self) -> bool {
        self.engaged
    }

    /// The integrator — the learned frequency word (ppm).
    pub fn integrator_ppm(&self) -> f64 {
        self.i_ppm
    }

    /// True between a grandmaster change and the window that re-anchors on it: `D` still belongs
    /// to the OLD time base, so it must not be published as the fleet offset.
    pub fn rebase_pending(&self) -> bool {
        self.rebase_pending
    }

    pub fn last_error_ns(&self) -> Option<i64> {
        self.last_error_ns
    }

    pub fn last_freq_ppm(&self) -> f64 {
        self.last_freq_ppm
    }

    /// Grandmaster frequency steps followed since start (camera-box issue 1372).
    pub fn freq_steps(&self) -> u32 {
        self.freq_steps
    }

    /// The last frequency step followed.
    pub fn last_freq_step(&self) -> Option<FreqStep> {
        self.last_freq_step
    }

    /// The phase (µs) still being retired after the last followed step; 0 when none.
    pub fn pull_us(&self) -> f64 {
        self.pull_us
    }

    /// Adopt `D` from the date-offset authority (a join or an absorb). The wall is moved by the
    /// caller when the adoption is a step; the anchor simply becomes the authority's value.
    pub fn set_anchor(&mut self, anchor_ns: i64) {
        self.anchor_ns = Some(anchor_ns);
    }

    /// The wall was stepped by `delta_ns`: `D` moves by exactly the same amount, so `e` is
    /// unchanged and the loop sees no disturbance. No-op before the first anchor.
    pub fn note_step(&mut self, delta_ns: i64) {
        if let Some(a) = self.anchor_ns.as_mut() {
            *a = a.wrapping_add(delta_ns);
        }
    }

    /// This box lost PTP: stop owning the frequency word (the integrator and the anchor are kept).
    /// The next locked window re-engages bumplessly from the rate servo's word, and re-anchors
    /// (`Realigned`) if the wall free-ran more than [`REANCHOR_ON_ENGAGE_NS`] meanwhile.
    pub fn disengage(&mut self) {
        self.engaged = false;
        self.drop_step_state();
    }

    /// The frequency-step ring and a running pull belong to one continuous engagement in one time
    /// base: dropped on a lock loss, an engagement and a re-anchor.
    fn drop_step_state(&mut self) {
        self.fstep.reset();
        self.pull_us = 0.0;
    }

    /// The grandmaster (or the sync source) changed: re-anchor `D` from the next window, so the
    /// wall stays continuous in the new grandmaster's time base.
    pub fn request_rebase(&mut self) {
        if self.anchor_ns.is_some() {
            self.rebase_pending = true;
        }
    }

    /// Feed one PTP sample window.
    ///
    /// - `median_diff_ns` — the median of `t2 − t1` over the window (raw, NOT the mod-1 s display
    ///   phase and NOT calibration-corrected: `D` is an absolute offset between two time bases).
    /// - `ptp_locked` — the controller's PTP lock verdict (the rate servo's rate-stability lock).
    /// - `rate_servo_freq_ppm` — the word the rate servo would apply now; the integrator starts
    ///   from it on engagement so the hand-over is bumpless.
    /// - `dt_s` — seconds since the previous window.
    ///
    /// No argument carries NTP: this is the whole frequency law while engaged.
    pub fn on_window(
        &mut self,
        median_diff_ns: i64,
        ptp_locked: bool,
        rate_servo_freq_ppm: f64,
        dt_s: f64,
    ) -> WindowOutcome {
        let mut event = AnchorEvent::None;
        let anchor = match self.anchor_ns {
            None => {
                if !ptp_locked {
                    return WindowOutcome {
                        freq_ppm: None,
                        error_ns: None,
                        event,
                        freq_step: None,
                    };
                }
                self.anchor_ns = Some(median_diff_ns);
                event = AnchorEvent::Anchored {
                    anchor_ns: median_diff_ns,
                };
                median_diff_ns
            }
            Some(a) => {
                let off = median_diff_ns.wrapping_sub(a).abs();
                if self.rebase_pending || off > DISCONTINUITY_NS {
                    self.rebase_pending = false;
                    self.anchor_ns = Some(median_diff_ns);
                    event = AnchorEvent::Rebased {
                        old_ns: a,
                        new_ns: median_diff_ns,
                    };
                    median_diff_ns
                } else if ptp_locked && !self.engaged && off > REANCHOR_ON_ENGAGE_NS {
                    self.anchor_ns = Some(median_diff_ns);
                    event = AnchorEvent::Realigned {
                        old_ns: a,
                        new_ns: median_diff_ns,
                    };
                    median_diff_ns
                } else {
                    a
                }
            }
        };

        let e_ns = median_diff_ns.wrapping_sub(anchor);
        self.last_error_ns = Some(e_ns);

        if !ptp_locked {
            if self.engaged {
                // Hand the learned frequency back to the rate servo (the controller copies
                // `integrator_ppm` into its drift baseline).
                self.engaged = false;
                self.drop_step_state();
            }
            return WindowOutcome {
                freq_ppm: None,
                error_ns: Some(e_ns),
                event,
                freq_step: None,
            };
        }

        if !self.engaged {
            self.engaged = true;
            self.i_ppm = rate_servo_freq_ppm.clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
            self.drop_step_state();
        }
        if event != AnchorEvent::None {
            self.drop_step_state();
        }

        let dt = if dt_s.is_finite() {
            dt_s.clamp(DT_MIN_S, DT_MAX_S)
        } else {
            DT_MIN_S
        };
        let e_raw_us = e_ns as f64 / 1_000.0;
        // camera-box issue 1372: a confirmed grandmaster frequency step re-seeds the integrator.
        let freq_step = self
            .fstep
            .observe(e_raw_us, dt_s, self.i_ppm)
            .map(|est| self.follow_freq_step(est, e_raw_us));
        let f = if self.pull_us != 0.0 {
            self.pulled_word(e_raw_us, dt)
        } else {
            let e_us = e_raw_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
            // offset = local − master: a fast local clock GROWS e, so the correction is negative.
            self.i_ppm =
                (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
            (self.i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
        };
        self.fstep.note_word(f);
        self.last_freq_ppm = f;
        WindowOutcome {
            freq_ppm: Some(f),
            error_ns: Some(e_ns),
            event,
            freq_step,
        }
    }

    /// Follow a confirmed step: the integrator jumps by the measured error (bounded), and the phase
    /// error the step left becomes the pull reference. No wall step, `D` untouched.
    fn follow_freq_step(&mut self, est: FreqStepEstimate, e_us: f64) -> FreqStep {
        let before = self.i_ppm;
        let step_ppm = (-est.error_ppm).clamp(-FSTEP_MAX_PPM, FSTEP_MAX_PPM);
        self.i_ppm = (before + step_ppm).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.pull_us = e_us.clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let followed = FreqStep {
            step_ppm,
            error_ppm: est.error_ppm,
            sigma_ppm: est.sigma_ppm,
            linearity_f: est.linearity_f,
            span_s: est.span_s,
            integrator_before_ppm: before,
            integrator_after_ppm: self.i_ppm,
            pull_us: self.pull_us,
        };
        self.freq_steps = self.freq_steps.saturating_add(1);
        self.last_freq_step = Some(followed);
        followed
    }

    /// The word while a followed step's phase is retired: the PI tracks `e − r` (so it sees no
    /// error when the phase follows the reference) and the word carries the reference's own rate
    /// `−r/τ` as feed-forward. The reference then decays by one window.
    fn pulled_word(&mut self, e_raw_us: f64, dt: f64) -> f64 {
        let e_us = (e_raw_us - self.pull_us).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        let pull_rate = -self.pull_us / FSTEP_PULL_TAU_S;
        self.i_ppm = (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        let f = (self.i_ppm - K_P_PER_S * e_us + pull_rate).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.pull_us *= (-dt / FSTEP_PULL_TAU_S).exp();
        if self.pull_us.abs() < FSTEP_PULL_DONE_US {
            self.pull_us = 0.0;
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 0.5;

    /// A closed-loop plant: `e` integrates the oscillator error plus the applied correction
    /// (µs/s = ppm), with `delay` windows of transport delay between command and effect (the
    /// phase-slew rule: a zero-delay sim hides oscillation).
    struct Plant {
        e_ns: f64,
        osc_ppm: f64,
        applied: std::collections::VecDeque<f64>,
    }

    impl Plant {
        fn new(osc_ppm: f64, delay: usize, initial_word: f64) -> Self {
            Plant {
                e_ns: 0.0,
                osc_ppm,
                applied: std::iter::repeat(initial_word).take(delay + 1).collect(),
            }
        }
        fn advance(&mut self, cmd: f64, dt: f64) {
            self.applied.push_back(cmd);
            let f = self.applied.pop_front().unwrap();
            self.e_ns += (self.osc_ppm + f) * dt * 1_000.0;
        }
    }

    const D: i64 = 1_789_000_000_000_000_000;

    fn run(core: &mut PhaseLockCore, plant: &mut Plant, n: usize, word0: f64) -> (f64, f64) {
        let mut max_late = 0.0f64;
        let mut last = word0;
        for i in 0..n {
            let out = core.on_window(D + plant.e_ns.round() as i64, true, word0, DT);
            last = out.freq_ppm.expect("engaged");
            plant.advance(last, DT);
            if i > n / 2 {
                max_late = max_late.max(plant.e_ns.abs());
            }
        }
        (max_late, last)
    }

    #[test]
    fn it_does_not_engage_or_anchor_before_ptp_lock() {
        let mut c = PhaseLockCore::new();
        let out = c.on_window(D, false, 12.0, DT);
        assert_eq!(out.freq_ppm, None);
        assert_eq!(out.event, AnchorEvent::None);
        assert_eq!(c.anchor_ns(), None);
        assert!(!c.engaged());
    }

    #[test]
    fn the_first_lock_anchors_on_the_current_offset_and_takes_over_bumplessly() {
        let mut c = PhaseLockCore::new();
        let out = c.on_window(D, true, -23.5, DT);
        assert_eq!(out.event, AnchorEvent::Anchored { anchor_ns: D });
        assert_eq!(out.error_ns, Some(0));
        assert_eq!(
            out.freq_ppm,
            Some(-23.5),
            "e = 0 at the anchor: the word is unchanged"
        );
        assert!(c.engaged());
    }

    #[test]
    fn it_holds_phase_against_a_constant_oscillator_error_with_zero_steady_state() {
        // A +26 ppm oscillator, handed over with a word that is 0.4 ppm off (the rate servo's
        // residual) — the phase term must pull e back and the integrator learn −26 ppm exactly.
        for delay in [0usize, 1, 2] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(26.0, delay, -25.6);
            let (max_late, word) = run(&mut c, &mut p, 4_000, -25.6);
            assert!(max_late < 5_000.0, "delay {delay}: late |e| {max_late} ns");
            assert!(
                (word + 26.0).abs() < 0.01,
                "delay {delay}: word {word} ppm, want −26"
            );
            assert!((c.integrator_ppm() + 26.0).abs() < 0.01);
        }
    }

    #[test]
    fn there_is_no_deadband_a_tiny_rate_error_is_corrected_not_left_to_drift() {
        // The NANO deadband ignored |rate| < 0.1 µs/s forever (360 µs/h). The phase lock must
        // hold a 0.05 ppm residual to microseconds over an hour.
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(0.05, 1, 0.0);
        let (max_late, _) = run(&mut c, &mut p, 7_200, 0.0);
        assert!(
            max_late < 1_000.0,
            "late |e| {max_late} ns after an hour at 0.05 ppm"
        );
    }

    #[test]
    fn a_step_of_the_wall_with_the_same_step_of_d_is_invisible_to_the_loop() {
        let mut a = PhaseLockCore::new();
        let mut b = PhaseLockCore::new();
        let mut pa = Plant::new(7.0, 1, 0.0);
        let mut pb = Plant::new(7.0, 1, 0.0);
        let step: i64 = 51_234_567;
        let mut words_a = Vec::new();
        let mut words_b = Vec::new();
        for i in 0..400 {
            if i == 200 {
                b.note_step(step); // D moves with the wall …
            }
            let off_b = if i >= 200 { step } else { 0 }; // … and so does t2
            let wa = a
                .on_window(D + pa.e_ns.round() as i64, true, 0.0, DT)
                .freq_ppm
                .unwrap();
            let wb = b
                .on_window(D + pb.e_ns.round() as i64 + off_b, true, 0.0, DT)
                .freq_ppm
                .unwrap();
            pa.advance(wa, DT);
            pb.advance(wb, DT);
            words_a.push(wa);
            words_b.push(wb);
        }
        assert_eq!(
            words_a, words_b,
            "bit-identical frequency commands across a date step"
        );
    }

    #[test]
    fn a_grandmaster_change_re_anchors_without_disturbing_the_frequency() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-12.0, 1, 12.0);
        run(&mut c, &mut p, 2_000, 12.0);
        let word_before = c.last_freq_ppm();
        c.request_rebase();
        // The new GM's uptime is 3.7 days behind: t2 − t1 jumps by +3.7 days, the wall does not.
        let jump: i64 = 320_000 * 1_000_000_000;
        let out = c.on_window(D + jump + p.e_ns.round() as i64, true, 0.0, DT);
        match out.event {
            AnchorEvent::Rebased { old_ns, new_ns } => {
                assert_eq!(old_ns, D);
                assert_eq!(new_ns, D + jump + p.e_ns.round() as i64);
            }
            other => panic!("expected a rebase, got {other:?}"),
        }
        assert_eq!(out.error_ns, Some(0));
        assert!(
            (out.freq_ppm.unwrap() - word_before).abs() < 0.01,
            "no frequency kick"
        );
    }

    #[test]
    fn a_time_base_jump_without_a_uuid_change_is_detected_as_a_discontinuity() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 0.0, DT);
        let out = c.on_window(D - 2 * DISCONTINUITY_NS, true, 0.0, DT);
        assert!(matches!(out.event, AnchorEvent::Rebased { .. }));
        assert_eq!(out.error_ns, Some(0));
        // Just under the threshold is an (absurd) phase error, not a rebase — clamped.
        let out = c.on_window(D - 2 * DISCONTINUITY_NS + DISCONTINUITY_NS, true, 0.0, DT);
        assert_eq!(out.event, AnchorEvent::None);
        assert!(out.freq_ppm.unwrap().abs() <= K_P_PER_S * ERROR_CLAMP_US + 1e-9 + 1.0);
    }

    #[test]
    fn losing_lock_disengages_and_keeps_the_learned_frequency_for_the_hand_back() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(30.0, 1, -30.0);
        run(&mut c, &mut p, 2_000, -30.0);
        let out = c.on_window(D + p.e_ns.round() as i64, false, 0.0, DT);
        assert_eq!(out.freq_ppm, None);
        assert!(!c.engaged());
        assert!((c.integrator_ppm() + 30.0).abs() < 0.05);
        // Re-engaging takes the (rate servo's) word, and the anchor survived.
        let out = c.on_window(D + p.e_ns.round() as i64, true, -29.0, DT);
        assert!(out.freq_ppm.is_some());
        assert_eq!(c.anchor_ns(), Some(D));
    }

    #[test]
    fn re_engaging_far_off_the_anchor_re_anchors_instead_of_slewing() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 5.0, DT);
        c.on_window(D, false, 5.0, DT); // lock lost: the rate servo holds the clock …
                                        // … and it free-ran 3 ms away. Re-engaging must not slew 3 ms at the P clamp.
        let out = c.on_window(D + 3_000_000, true, 5.0, DT);
        assert_eq!(
            out.event,
            AnchorEvent::Realigned {
                old_ns: D,
                new_ns: D + 3_000_000
            },
            "same time base: a re-alignment, not a rebase of the fleet offset"
        );
        assert_eq!(out.error_ns, Some(0));
        assert!(
            (out.freq_ppm.unwrap() - 5.0).abs() < 1e-9,
            "bumpless, no slew"
        );
        // A small re-engagement error is simply tracked.
        c.on_window(D + 3_000_000, false, 5.0, DT);
        let out = c.on_window(D + 3_000_000 + 400_000, true, 5.0, DT);
        assert_eq!(out.event, AnchorEvent::None);
        assert_eq!(out.error_ns, Some(400_000));
    }

    #[test]
    fn disengage_keeps_the_anchor_and_the_learned_frequency() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(-18.0, 1, 18.0);
        run(&mut c, &mut p, 2_000, 18.0);
        c.disengage();
        assert!(!c.engaged());
        assert_eq!(c.anchor_ns(), Some(D));
        assert!((c.integrator_ppm() - 18.0).abs() < 0.05);
        // After a 3 ms free-run the next locked window re-aligns instead of slewing.
        let out = c.on_window(D + 3_000_000, true, 18.0, DT);
        assert!(matches!(out.event, AnchorEvent::Realigned { .. }));
        assert!(c.engaged());
    }

    #[test]
    fn rebase_pending_is_visible_until_the_re_anchoring_window() {
        let mut c = PhaseLockCore::new();
        assert!(!c.rebase_pending());
        c.request_rebase();
        assert!(
            !c.rebase_pending(),
            "nothing to rebase before the first anchor"
        );
        c.on_window(D, true, 0.0, DT);
        c.request_rebase();
        assert!(c.rebase_pending());
        c.on_window(D + 7, true, 0.0, DT);
        assert!(!c.rebase_pending());
    }

    #[test]
    fn the_error_clamp_bounds_the_response_to_a_huge_error() {
        let mut c = PhaseLockCore::new();
        c.on_window(D, true, 0.0, DT);
        let out = c.on_window(D + 900_000_000, true, 0.0, DT); // 900 ms (< discontinuity)
        let f = out.freq_ppm.unwrap();
        assert!(f.abs() <= K_P_PER_S * ERROR_CLAMP_US + K_I_PER_S2 * ERROR_CLAMP_US * DT + 1e-9);
    }

    #[test]
    fn gains_are_critically_damped() {
        let zeta = K_P_PER_S / (2.0 * K_I_PER_S2.sqrt());
        assert!((zeta - 1.0).abs() < 1e-12);
    }

    // ---- camera-box issue 1372: a grandmaster FREQUENCY step --------------------------------

    /// The law before the frequency-step follow (the plain PI), for the bit-for-bit comparison.
    fn plain_pi(i_ppm: &mut f64, e_ns: i64, dt: f64) -> f64 {
        let e_us = (e_ns as f64 / 1_000.0).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        *i_ppm = (*i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        (*i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM)
    }

    #[test]
    fn without_a_confirmed_step_the_words_are_the_plain_pi_bit_for_bit_1372() {
        // A 0.4 ppm hand-over error and a 0.1 ppm/min oscillator ramp for 50 minutes.
        for delay in [0usize, 1, 2] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(26.0, delay, -25.6);
            let mut q = Plant::new(26.0, delay, -25.6);
            let mut i_ref = -25.6;
            for n in 0..6_000 {
                let w = c
                    .on_window(D + p.e_ns.round() as i64, true, -25.6, DT)
                    .freq_ppm
                    .unwrap();
                let w_ref = plain_pi(&mut i_ref, q.e_ns.round() as i64, DT);
                assert_eq!(w.to_bits(), w_ref.to_bits(), "delay {delay}, window {n}");
                let osc = 26.0 + 0.1 * (n as f64 * DT) / 60.0;
                p.osc_ppm = osc;
                q.osc_ppm = osc;
                p.advance(w, DT);
                q.advance(w_ref, DT);
            }
            assert_eq!(c.freq_steps(), 0);
        }
    }

    #[test]
    fn a_frequency_step_re_seeds_the_integrator_in_seconds_and_never_moves_d_1372() {
        for delay in [0usize, 1] {
            let mut c = PhaseLockCore::new();
            let mut p = Plant::new(23.0, delay, -23.0);
            run(&mut c, &mut p, 1_200, -23.0); // 10 minutes locked
            let anchor = c.anchor_ns();
            // The grandmaster speeds up by 25 ppm: the oscillator relative to it drops by 25.
            p.osc_ppm -= 25.0;
            let mut followed = Vec::new();
            for n in 0..1_200 {
                let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
                assert_eq!(out.event, AnchorEvent::None, "delay {delay}");
                if let Some(fs) = out.freq_step {
                    followed.push((n, fs));
                }
                p.advance(out.freq_ppm.unwrap(), DT);
            }
            assert_eq!(followed.len(), 1, "delay {delay}: {followed:?}");
            let (n, fs) = followed[0];
            assert!(
                (n + 1) as f64 * DT <= 30.0,
                "delay {delay}: after {n} windows"
            );
            assert!((fs.step_ppm - 25.0).abs() < 1.0, "delay {delay}: {fs:?}");
            assert_eq!(fs.step_ppm, -fs.error_ppm);
            // The integrator lands on the new frequency (−osc = +2 ppm) at once.
            assert!(
                (fs.integrator_after_ppm - 2.0).abs() < 0.5,
                "delay {delay}: {fs:?}"
            );
            assert!(fs.pull_us < -300.0, "the step left ~−400 µs: {fs:?}");
            assert_eq!(c.last_freq_step(), Some(fs));
            assert_eq!(c.freq_steps(), 1);
            // D never moved; the pull finished; the phase and the frequency are back.
            assert_eq!(c.anchor_ns(), anchor, "delay {delay}");
            assert_eq!(c.pull_us(), 0.0);
            assert!(p.e_ns.abs() < 5_000.0, "delay {delay}: e {} ns", p.e_ns);
            assert!((c.integrator_ppm() - 2.0).abs() < 0.05, "delay {delay}");
        }
    }

    #[test]
    fn the_pull_retires_the_step_phase_without_disturbing_the_integrator_1372() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(23.0, 0, -23.0);
        run(&mut c, &mut p, 1_200, -23.0);
        p.osc_ppm -= 25.0;
        let mut after = None;
        let mut worst_i_err = 0.0f64;
        for n in 0..1_200 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            if out.freq_step.is_some() {
                after = Some(n);
            }
            if after.is_some() {
                worst_i_err = worst_i_err.max((c.integrator_ppm() - 2.0).abs());
            }
            p.advance(out.freq_ppm.unwrap(), DT);
            if let Some(a) = after {
                if n == a + 240 {
                    // Two minutes after the re-seed the phase is back under 50 µs …
                    assert!(p.e_ns.abs() < 50_000.0, "e {} ns", p.e_ns);
                }
            }
        }
        assert!(after.is_some());
        // … and the integrator never left the new frequency by more than a few tenths of a ppm
        // (a plain PI trimming ~400 µs would swing it by ~1.5 ppm).
        assert!(worst_i_err < 0.5, "{worst_i_err}");
    }

    #[test]
    fn a_lock_loss_or_a_re_anchor_drops_the_pull_and_the_ring_1372() {
        let mut c = PhaseLockCore::new();
        let mut p = Plant::new(23.0, 0, -23.0);
        run(&mut c, &mut p, 1_200, -23.0);
        p.osc_ppm -= 25.0;
        for _ in 0..200 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
            if out.freq_step.is_some() {
                break;
            }
        }
        assert!(c.pull_us() != 0.0, "a pull is running");
        // Refill the ring for a few windows, so that dropping it is observable.
        for _ in 0..10 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
        }
        assert!(c.fstep.points() >= 9, "{}", c.fstep.points());
        // Lock lost: nothing is pulled any more and the ring is gone.
        c.on_window(D + p.e_ns.round() as i64, false, 0.0, DT);
        assert_eq!(c.pull_us(), 0.0);
        assert_eq!(c.fstep.points(), 0);
        // Re-engaged, the ring refills …
        for _ in 0..10 {
            let out = c.on_window(D + p.e_ns.round() as i64, true, 0.0, DT);
            p.advance(out.freq_ppm.unwrap(), DT);
        }
        assert!(c.fstep.points() >= 9);
        // … and a grandmaster change re-anchors: the ring starts again from that window.
        c.request_rebase();
        let out = c.on_window(D + 7 * DISCONTINUITY_NS, true, 2.0, DT);
        assert!(matches!(out.event, AnchorEvent::Rebased { .. }));
        assert_eq!(c.pull_us(), 0.0);
        assert!(c.fstep.points() <= 1, "{}", c.fstep.points());
        assert_eq!(c.freq_steps(), 1, "the count survives");
    }
}
