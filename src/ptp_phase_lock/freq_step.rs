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
//! The line fit runs on every engaged window without allocating; the split scan runs only for a
//! window whose slope already passes the first test (never in the steady state).
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

/// A line fitted to the ring of `(t, p)` points, with its linearity test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RingFit {
    /// The slope of `p` (µs/s = ppm).
    pub slope_ppm: f64,
    /// The slope's standard error (ppm).
    pub sigma_ppm: f64,
    /// The largest partial F of a level shift or a slope change inside the ring.
    pub linearity_f: f64,
    /// The slope of the line fitted together with the best level-shift split (ppm): what is left
    /// of the slope once the most likely level shift is explained away.
    pub shifted_slope_ppm: f64,
    pub points: usize,
    pub span_s: f64,
}

/// The least-squares line of a ring, from sums only (no allocation: it runs on every window).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineFit {
    pub slope_ppm: f64,
    pub sigma_ppm: f64,
    pub points: usize,
    pub span_s: f64,
    t_mean: f64,
    p_mean: f64,
    sxx: f64,
    sse: f64,
}

/// Fit a line to `(t s, p µs)` points (time strictly increasing). `None` below
/// [`FSTEP_MIN_POINTS`] or when the times do not spread.
pub fn fit_line<'a>(points: impl Iterator<Item = &'a (f64, f64)> + Clone) -> Option<LineFit> {
    let (mut n, mut t_sum, mut p_sum) = (0usize, 0.0f64, 0.0f64);
    let (mut t_first, mut t_last) = (0.0f64, 0.0f64);
    for &(t, p) in points.clone() {
        if n == 0 {
            t_first = t;
        }
        t_last = t;
        n += 1;
        t_sum += t;
        p_sum += p;
    }
    if n < FSTEP_MIN_POINTS {
        return None;
    }
    let nf = n as f64;
    let (t_mean, p_mean) = (t_sum / nf, p_sum / nf);
    let (mut sxx, mut sxy, mut syy) = (0.0f64, 0.0f64, 0.0f64);
    for &(t, p) in points {
        let (x, y) = (t - t_mean, p - p_mean);
        sxx += x * x;
        sxy += x * y;
        syy += y * y;
    }
    if !sxx.is_finite() || sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let sse = (syy - slope * sxy).max(0.0);
    let floor = FSTEP_FIT_FLOOR_US * FSTEP_FIT_FLOOR_US;
    Some(LineFit {
        slope_ppm: slope,
        sigma_ppm: ((sse / (nf - 2.0)).max(floor) / sxx).sqrt(),
        points: n,
        span_s: t_last - t_first,
        t_mean,
        p_mean,
        sxx,
        sse,
    })
}

/// The linearity test of a fitted line: the largest partial F of a level shift or a slope change
/// at any split keeping [`FSTEP_SPLIT_EDGE`] points on each side, and the slope with the best
/// level shift explained. Returns `(linearity_f, shifted_slope_ppm)`.
pub fn split_test(points: &[(f64, f64)], line: &LineFit) -> (f64, f64) {
    let n = points.len();
    if n != line.points || n < 2 * FSTEP_SPLIT_EDGE {
        return (f64::INFINITY, line.slope_ppm);
    }
    let nf = n as f64;
    let sxx = line.sxx;
    // Suffix sums over i >= k of: 1, t, t², r, t·r (t centered, r the line's residual).
    let mut s_n = vec![0.0; n + 1];
    let mut s_t = vec![0.0; n + 1];
    let mut s_tt = vec![0.0; n + 1];
    let mut s_r = vec![0.0; n + 1];
    let mut s_tr = vec![0.0; n + 1];
    for i in (0..n).rev() {
        let t = points[i].0 - line.t_mean;
        let r = points[i].1 - line.p_mean - line.slope_ppm * t;
        s_n[i] = s_n[i + 1] + 1.0;
        s_t[i] = s_t[i + 1] + t;
        s_tt[i] = s_tt[i + 1] + t * t;
        s_r[i] = s_r[i + 1] + r;
        s_tr[i] = s_tr[i + 1] + t * r;
    }
    // Adding one regressor x to the line: its coefficient is (Σ x·r) / |x̃|², the residual sum of
    // squares falls by (Σ x·r)² / |x̃|², where x̃ is x without its projection on (1, t) — the
    // residuals are already orthogonal to (1, t), so Σ x̃·r = Σ x·r.
    let residualized = |sum_x: f64, sum_xx: f64, sum_xt: f64| -> f64 {
        sum_xx - sum_x * sum_x / nf - sum_xt * sum_xt / sxx
    };
    let mut best = 0.0f64;
    let (mut best_shift, mut shifted_slope) = (0.0f64, line.slope_ppm);
    for k in FSTEP_SPLIT_EDGE..=(n - FSTEP_SPLIT_EDGE) {
        let nk = s_n[k];
        // A level shift: x = 1 for i >= k. With it, the slope becomes b − c·Σx·t / Sxx.
        let den = residualized(nk, nk, s_t[k]);
        if den > 1e-9 {
            let reduction = s_r[k] * s_r[k] / den;
            best = best.max(reduction);
            if reduction > best_shift {
                best_shift = reduction;
                shifted_slope = line.slope_ppm - (s_r[k] / den) * s_t[k] / sxx;
            }
        }
        // A slope change at the knot t[k-1]: x = t − t[k-1] for i >= k.
        let knot = points[k - 1].0 - line.t_mean;
        let sh = s_t[k] - nk * knot;
        let shh = s_tt[k] - 2.0 * knot * s_t[k] + nk * knot * knot;
        let sht = s_tt[k] - knot * s_t[k];
        let shr = s_tr[k] - knot * s_r[k];
        let den = residualized(sh, shh, sht);
        if den > 1e-9 {
            best = best.max(shr * shr / den);
        }
    }
    let floor = FSTEP_FIT_FLOOR_US * FSTEP_FIT_FLOOR_US;
    let rest = ((line.sse - best) / (nf - 3.0)).max(floor);
    (best / rest, shifted_slope)
}

/// Fit a line to `(t s, p µs)` points (time strictly increasing) and test it for linearity.
/// `None` below [`FSTEP_MIN_POINTS`] or when the times do not spread.
pub fn fit_ring(points: &[(f64, f64)]) -> Option<RingFit> {
    let line = fit_line(points.iter())?;
    let (linearity_f, shifted_slope_ppm) = split_test(points, &line);
    Some(RingFit {
        slope_ppm: line.slope_ppm,
        sigma_ppm: line.sigma_ppm,
        linearity_f,
        shifted_slope_ppm,
        points: line.points,
        span_s: line.span_s,
    })
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
        let elapsed = if dt_s.is_finite() {
            dt_s.clamp(0.0, DT_MAX_S)
        } else {
            0.0
        };
        self.holdoff_s = (self.holdoff_s - elapsed).max(0.0);
        let last_word = match self.last_word_ppm {
            Some(w) if (DT_MIN_S..=DT_MAX_S).contains(&dt_s) => w,
            _ => {
                // First window after a restart, or a gap the ring cannot represent: restart here.
                self.reset();
                if e_us.is_finite() {
                    self.ring.push_back((0.0, e_us));
                }
                return None;
            }
        };
        if !e_us.is_finite() {
            self.reset();
            return None;
        }
        self.clock_s += dt_s;
        self.applied_us += last_word * dt_s;
        let now = self.clock_s;
        self.ring.push_back((now, e_us - self.applied_us));
        while self.ring.len() > FSTEP_MAX_POINTS
            || self
                .ring
                .front()
                .is_some_and(|&(t, _)| now - t > FSTEP_WINDOW_S + 1e-9)
        {
            self.ring.pop_front();
        }
        let judged = self.holdoff_s <= 0.0 && self.clock_s >= FSTEP_WINDOW_S - 1e-9;
        let line = if judged {
            fit_line(self.ring.iter())
        } else {
            None
        };
        let Some(line) = line else {
            self.run_sign = 0;
            self.run_len = 0;
            return None;
        };
        let error = line.slope_ppm + integrator_ppm;
        // The cheap test first: the split scan runs only for a window that already has a step's
        // slope, so the steady state never pays for it.
        let slope_says_step =
            error.abs() >= FSTEP_MIN_PPM && error.abs() >= FSTEP_SIGMAS * line.sigma_ppm;
        let (linearity_f, _shifted_slope) = if slope_says_step {
            split_test(self.ring.make_contiguous(), &line)
        } else {
            (f64::INFINITY, line.slope_ppm)
        };
        let candidate = slope_says_step && linearity_f <= FSTEP_LINEARITY_F_MAX;
        if !candidate {
            self.run_sign = 0;
            self.run_len = 0;
            return None;
        }
        let sign: i8 = if error > 0.0 { 1 } else { -1 };
        if sign != self.run_sign {
            self.run_sign = sign;
            self.run_len = 0;
        }
        self.run_len += 1;
        if self.run_len < FSTEP_CONFIRM {
            return None;
        }
        self.reset();
        self.holdoff_s = FSTEP_HOLDOFF_S;
        Some(FreqStepEstimate {
            error_ppm: error,
            sigma_ppm: line.sigma_ppm,
            linearity_f,
            span_s: line.span_s,
            points: line.points,
        })
    }
}

#[cfg(test)]
mod tests;
