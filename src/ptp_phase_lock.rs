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
//! Pure (explicit inputs, no I/O, no logging) so the controller and the bench run the same code.

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

/// `dt` is clamped to this range (s): a first update or a long gap must not scale the integrator
/// step by an unbounded interval.
pub const DT_MIN_S: f64 = 0.01;
pub const DT_MAX_S: f64 = 5.0;

/// What happened to the anchor `D` in one window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnchorEvent {
    None,
    /// First anchor (the first PTP lock).
    Anchored {
        anchor_ns: i64,
    },
    /// Re-anchored from the continuous wall: a grandmaster change, or a time-base jump.
    Rebased {
        old_ns: i64,
        new_ns: i64,
    },
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

    pub fn last_error_ns(&self) -> Option<i64> {
        self.last_error_ns
    }

    pub fn last_freq_ppm(&self) -> f64 {
        self.last_freq_ppm
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
                    };
                }
                self.anchor_ns = Some(median_diff_ns);
                event = AnchorEvent::Anchored {
                    anchor_ns: median_diff_ns,
                };
                median_diff_ns
            }
            Some(a) => {
                if self.rebase_pending || median_diff_ns.wrapping_sub(a).abs() > DISCONTINUITY_NS {
                    self.rebase_pending = false;
                    self.anchor_ns = Some(median_diff_ns);
                    event = AnchorEvent::Rebased {
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
            }
            return WindowOutcome {
                freq_ppm: None,
                error_ns: Some(e_ns),
                event,
            };
        }

        if !self.engaged {
            self.engaged = true;
            self.i_ppm = rate_servo_freq_ppm.clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        }

        let dt = if dt_s.is_finite() {
            dt_s.clamp(DT_MIN_S, DT_MAX_S)
        } else {
            DT_MIN_S
        };
        let e_us = (e_ns as f64 / 1_000.0).clamp(-ERROR_CLAMP_US, ERROR_CLAMP_US);
        // offset = local − master: a fast local clock GROWS e, so the correction is negative.
        self.i_ppm = (self.i_ppm - K_I_PER_S2 * e_us * dt).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        let f = (self.i_ppm - K_P_PER_S * e_us).clamp(-FREQ_CLAMP_PPM, FREQ_CLAMP_PPM);
        self.last_freq_ppm = f;
        WindowOutcome {
            freq_ppm: Some(f),
            error_ns: Some(e_ns),
            event,
        }
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
}
