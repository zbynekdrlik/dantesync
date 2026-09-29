//! The FREQUENCY-STEP detector of the phase lock (camera-box issue 1372, dantesync slice).
//!
//! # Why
//!
//! A Dante leader re-election can move the grandmaster's frequency by ~25 ppm at once (the
//! AIC128-D card and the 10.77.7.106 device differ by that much, 29.9.2026). The phase lock's PI
//! is deliberately slow (critically damped, ~100 s), so it follows such a step in ~8-12 minutes,
//! and for all that time every OBS media clock is off the Dante tick while the Dante audio devices
//! followed at once. On the video VLAN the grandmaster keeps its identity through such a flip, so
//! no re-anchor path sees it.
//!
//! # What it measures
//!
//! The phase error alone cannot tell a frequency step from the loop's own correction: after a step
//! the proportional term pulls the phase back at a rate of its own, and re-engaging with a 1 ms
//! error slews 20 ppm. So the detector keeps the OPEN-LOOP phase
//!
//! ```text
//! p = e − ∫ word dt          (µs; the word the loop applied over each interval)
//! ```
//!
//! whose slope is the oscillator's rate against the grandmaster, whatever the loop commanded. The
//! frequency the loop has NOT learned is then `slope(p) + I` (the integrator `I` is minus the
//! learned oscillator error). It reads only `e` and the loop's own words: no NTP term.
//!
//! Over a ring of the last [`FSTEP_WINDOW_S`] of engaged windows it fits a line to `p`. A step is a
//! candidate when all of these hold:
//!
//! - the unlearned frequency is at least [`FSTEP_MIN_PPM`] and [`FSTEP_SIGMAS`] standard errors of
//!   the slope (the slope's own uncertainty, from the residual scatter);
//! - the ring is LINEAR: neither a level shift nor a slope change anywhere inside it explains the
//!   ring significantly better than one line (the largest partial F over every split, at most
//!   [`FSTEP_LINEARITY_F_MAX`]). A plain `|s| > 6σ` test passes a level shift (a median jump of A
//!   in the middle of the ring reads as a slope of 1.5·A/window with |s|/σ ≈ 1.7·√N), so a path
//!   delay change or a step's landing residual would otherwise be re-seeded as a false step. The
//!   same test waits until the ring holds only post-step windows, so the estimate is not biased by
//!   a kink inside it;
//!
//! and the candidate holds with the same sign for [`FSTEP_CONFIRM`] consecutive windows. A single
//! outlier cannot pass: its slope is at most √3 standard errors.
//!
//! After a confirmed step the ring is cleared and nothing is confirmed for [`FSTEP_HOLDOFF_S`], so
//! the loop cannot chase itself.
//!
//! Pure (explicit inputs, no I/O, no logging): the phase lock core owns one and the bench drives
//! the same code.

use std::collections::VecDeque;

/// The ring of engaged windows the slope is fitted over (s). 20 s of post-step data give a slope
/// standard error of ~0.3 ppm at the rig's timestamp noise, enough to re-seed within 1 ppm.
pub const FSTEP_WINDOW_S: f64 = 20.0;

/// The smallest unlearned frequency (ppm) that is a step. The PI follows oscillator wander (a
/// fraction of a ppm per minute) and a ramp leaves at most a few tenths of a ppm unlearned.
pub const FSTEP_MIN_PPM: f64 = 5.0;

/// The unlearned frequency must also be this many standard errors of the fitted slope.
pub const FSTEP_SIGMAS: f64 = 6.0;

/// The largest partial F of a level shift or a slope change inside the ring that still counts as
/// one line. Under white noise the largest over every split is below 15 in ~99 % of windows.
pub const FSTEP_LINEARITY_F_MAX: f64 = 15.0;

/// Consecutive candidate windows (0.5 s each) that confirm a step. Six (3 s) rather than three:
/// at 30 µs of sample noise it removes the occasional estimate taken from a ring that still held
/// a trace of the pre-step phase (worst settle to 1 ppm 146 s -> 22.5 s over 20 seeded runs).
pub const FSTEP_CONFIRM: u32 = 6;

/// After a confirmed step nothing is confirmed for this long (s).
pub const FSTEP_HOLDOFF_S: f64 = 30.0;

/// One re-seed moves the integrator by at most this (ppm); a larger step is followed in more than
/// one event.
pub const FSTEP_MAX_PPM: f64 = 100.0;

/// The ring needs at least this many windows for a fit (a split keeps [`FSTEP_SPLIT_EDGE`] on each
/// side); it holds at most [`FSTEP_MAX_POINTS`] (a bound if windows were ever much shorter).
pub const FSTEP_MIN_POINTS: usize = 12;
pub const FSTEP_MAX_POINTS: usize = 160;

/// A level shift / slope change is tried only where at least this many windows lie on each side.
pub const FSTEP_SPLIT_EDGE: usize = 3;

/// The residual scatter used by the fit is never below this (µs): software timestamps are never
/// better, and a noiseless (simulated) ring must not turn floating-point rounding into a verdict.
pub const FSTEP_FIT_FLOOR_US: f64 = 1.0;

/// `dt` outside this range (s) is a gap the ring cannot represent: the ring restarts. The same
/// bounds the phase lock clamps its own `dt` to.
const DT_MIN_S: f64 = super::DT_MIN_S;
const DT_MAX_S: f64 = super::DT_MAX_S;

/// A line fitted to the ring of `(t, p)` points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingFit {
    /// The slope of `p` (µs/s = ppm).
    pub slope_ppm: f64,
    /// The slope's standard error (ppm).
    pub sigma_ppm: f64,
    /// The largest partial F of a level shift or a slope change inside the ring.
    pub linearity_f: f64,
    pub points: usize,
    pub span_s: f64,
}

/// Fit a line to `(t s, p µs)` points (time strictly increasing) and test it for linearity.
/// `None` below [`FSTEP_MIN_POINTS`] or when the times do not spread.
pub fn fit_ring(points: &[(f64, f64)]) -> Option<RingFit> {
    let _ = points;
    None
}

/// A confirmed frequency step, as measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FreqStepEstimate {
    /// The frequency the loop has not learned (ppm): `slope(p) + I`. The word must move by minus
    /// this.
    pub error_ppm: f64,
    pub sigma_ppm: f64,
    pub linearity_f: f64,
    pub span_s: f64,
    pub points: usize,
}

/// The detector state of one phase lock.
#[derive(Clone, Debug, Default)]
pub struct FreqStepDetector {
    /// `(t, p)`: seconds since the ring restarted, the open-loop phase (µs).
    ring: VecDeque<(f64, f64)>,
    clock_s: f64,
    /// ∫ word dt since the ring restarted (µs).
    applied_us: f64,
    /// The word applied since the last window; `None` right after a restart.
    last_word_ppm: Option<f64>,
    holdoff_s: f64,
    run_sign: i8,
    run_len: u32,
}

impl FreqStepDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the ring (an engagement, a re-anchor, a lock loss): the next window starts a new one.
    /// The holdoff stays.
    pub fn reset(&mut self) {
        self.ring.clear();
        self.clock_s = 0.0;
        self.applied_us = 0.0;
        self.last_word_ppm = None;
        self.run_sign = 0;
        self.run_len = 0;
    }

    /// Windows currently in the ring.
    pub fn points(&self) -> usize {
        self.ring.len()
    }

    /// Seconds left of the holdoff after the last confirmed step.
    pub fn holdoff_s(&self) -> f64 {
        self.holdoff_s
    }

    /// The word the loop applies from now until the next window (ppm).
    pub fn note_word(&mut self, word_ppm: f64) {
        self.last_word_ppm = word_ppm.is_finite().then_some(word_ppm);
    }

    /// Feed one ENGAGED window, before the loop updates on it: its phase error `e_us`, the seconds
    /// since the previous window `dt_s` (grandmaster time) and the loop's integrator. Returns the
    /// step once confirmed (the ring is then cleared and the holdoff starts).
    pub fn observe(
        &mut self,
        e_us: f64,
        dt_s: f64,
        integrator_ppm: f64,
    ) -> Option<FreqStepEstimate> {
        let _ = (e_us, dt_s, integrator_ppm);
        None
    }
}

#[cfg(test)]
mod tests;
