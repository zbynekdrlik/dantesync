//! Bounded PI phase-slew servo (issue #97).
//!
//! Replaces the discrete NTP micro-step for a *small* (sub-50ms) UTC phase error with a bounded,
//! rate-limited FREQUENCY slew (`f_phase`), composed with the PTP frequency word into ONE
//! composite word `f_total = f_ptp + f_phase`, so the correction is applied through the single
//! existing `SystemClock::adjust_frequency` path on every platform.
//!
//! # Why two servos on one clock is safe here (the amendment to "do not add a second servo")
//!
//! `clock-discipline-and-testing.md` states the pre-#97 rule: any frequency offset injected to
//! correct UTC phase is read back by the PTP servo as drift and cancelled within seconds, so the
//! two loops fight. #97 makes it safe via **feed-forward decoupling** (the carrier line): the
//! commanded `f_phase` is SUBTRACTED from every PTP rate observation before the PTP servo consumes
//! it (`decouple_ptp_rate`), so the PTP servo never sees the slew as grandmaster disagreement and
//! cannot fight it. The PTP servo keeps the local oscillator locked to the Dante grandmaster's
//! rate; the phase servo independently trims UTC phase. Time-constant separation reinforces this:
//! the PTP servo works in fractions of a second, the phase servo's integrator in minutes.
//!
//! # Sign convention (derived from the code, never assumed)
//!
//! The controller's phase offset is `offset_us = t2%1e9 - t1%1e9 = local - master`, so a faster
//! local clock makes the observed offset GROW, i.e. `d(raw_rate)/d(applied_freq) = +1` (ppm per
//! ppm) — that is why the decoupling SUBTRACTS `f_phase`. The NTP phase error `e` is the NTP
//! offset in µs, positive when the local clock is BEHIND UTC (`step_clock(+)` moves it forward),
//! so `e > 0` means "speed the clock up" and the proportional term is `+k_p * e` (no negation).
//!
//! # The math is Tier-0 testable in isolation
//!
//! This whole module is platform-independent (no `#[cfg]`), so — exactly like `spike_filter`,
//! `gm_filter` and the `JitterEstimator` — its PI law, caps, anti-windup, deadband and closed-loop
//! convergence get real test execution. The controller only ever CALLS it.

use log::warn;

// ============================================================================
// SERVO CONSTANTS (issue #97 ratified design + the derived stability/robustness bounds)
// ============================================================================

/// Proportional gain: 0.02 ppm/µs. Loop gain `k_p·dt = 0.2` at the nominal 10 s cadence —
/// OVERDAMPED for the real one-sample-delay plant (the poles of `z²−z+0.2` are real: 0.72, 0.28).
/// #103 dropped this ~5× from the deadbeat `0.1` (`k_p·dt = 1.0`), which sat on the stability
/// boundary and — with the real ≈1-sample loop delay — rang into a sustained limit cycle in prod.
pub const K_P_PPM_PER_US: f64 = 0.02;

/// Integral gain: ppm accrued per (µs of error × second). #103 halved it (was 0.002); the rate
/// limit below is the binding term, and the integrator now absorbs the master's constant
/// Dante-vs-UTC error (≈23 ppm) over minutes — deliberately slow, so it never lurches the frequency.
pub const K_I_PPM_PER_US_S: f64 = 0.0008;

/// Integrator rate limit: the I-term may change by at most this many ppm per second. #103 lowered
/// it ~7× (was 1.0). At the ≈23 ppm inflow the integrator reaches equilibrium in ~150 s (minutes) —
/// slow and stable — while never letting a single noisy sample lurch the frequency.
pub const I_RATE_PPM_PER_S: f64 = 0.15;

/// Hard clamp on the integrator state (design: `clamp ±250ppm`). A secondary bound below the
/// composite `F_PHASE_CAP_PPM` (which, with anti-windup, binds first in normal operation).
pub const I_CLAMP_PPM: f64 = 250.0;

/// Hard cap on the total commanded phase slew (design: `celkový cap |f_phase| ≤ 200ppm`).
pub const F_PHASE_CAP_PPM: f64 = 200.0;

/// #103 — output slew-rate limiter: the commanded `f_phase` may change by at most this many ppm per
/// second, so a noisy proportional swing cannot lurch the clock's frequency (it bounds clock
/// ACCELERATION). At the low `k_p` this is a belt-and-suspenders that only binds on a large
/// transient (e.g. an error near the step boundary) — ramping the correction in smoothly instead of
/// slamming the full slew on in one step. `1.5 ppm/s` = at most 15 ppm/step at the 10 s cadence.
pub const F_PHASE_SLEW_RATE_PPM_PER_S: f64 = 1.5;

/// A phase error larger than this is NOT slewed — it is a cold boot / insane clock and the caller
/// STEPS instead (design: `STEP len |e| > 50ms`). 50 ms in µs.
pub const STEP_BOUNDARY_US: i64 = 50_000;

/// Damped ceiling on the EFFECTIVE proportional loop gain per update: `k_p_eff = min(k_p,
/// MAX_LOOP_GAIN / dt)`, so the loop gain `k_p_eff·dt ≤ MAX_LOOP_GAIN` at ANY cadence. #103 renamed
/// this from `DEADBEAT_MAX_GAIN` and lowered it `1.0 → 0.25`: the old deadbeat ceiling
/// (`k_p_eff·dt = 1.0`) drove the P error to zero in one step with ZERO stability margin, which the
/// real ≈1-sample loop delay turned into a sustained limit cycle. `0.25` is the critical-damping
/// ceiling (`z²−z+0.25` has a double real pole at 0.5), so the loop stays overdamped-or-critical at
/// every cadence. At the nominal ≤12.5 s cadence the design `k_p = 0.02` binds (`k_p·dt = 0.2` at
/// 10 s); only a slower cadence engages the ceiling — e.g. the client's 30 s: `k_p_eff = 0.25/30`,
/// critically damped, instead of the old unstable-with-delay deadbeat.
pub const MAX_LOOP_GAIN: f64 = 0.25;

/// FULL phase deadband (µs): inside this band the servo makes no NEW response to the sub-deadband
/// error — the proportional term is zero and the integrator STATE is frozen — so the NTP-path noise
/// floor is never chased. The integrator's already-absorbed DC frequency KEEPS being applied (that
/// is what holds a stable clock on-phase against the Dante-vs-UTC drift; zeroing `f_phase` in-band
/// would let that drift repop — the #103 failure); only the *reaction* to the residual is
/// suppressed. #103 renamed this from `I_DEADBAND_US` (which froze only the integrator) and
/// raised it `150 → 200`: 200 µs sits safely above the measurement noise floor (≈40 µs burst spread
/// and ≈130 µs inter-burst jitter; the healthy 6-sample spread was 119-135 µs) and below the master's
/// proportional equilibrium, so a real sustained DC error still pushes `|e|` past it and engages the
/// servo, while a stable clock is held (via the frozen integrator) without micro-chasing the jitter.
pub const PHASE_DEADBAND_US: i64 = 200;

/// Saturation-alarm error threshold (µs): part of the "slew saturated" guard.
pub const SAT_ALARM_E_US: i64 = 10_000;

/// Saturation-alarm dwell (seconds): `f_phase` capped AND `|e| > SAT_ALARM_E_US` must persist this
/// long before the alarm raises (design: `f_phase saturovaný AND |e|>10ms 60s → alarm`).
pub const SAT_ALARM_DWELL_S: f64 = 60.0;

// ============================================================================
// #105 — ACQUISITION mode (fast catch of the frequency DC) + convergence latch
// ============================================================================
//
// The #103 tracking gains above are deliberately slow so the STEADY-STATE loop never oscillates.
// But on a high-DC box (~50 ppm Dante-vs-UTC, e.g. cam1) the slow integrator cannot build the ~50 ppm
// holding frequency before the offset runs away, and a step then resets it — the #105 runaway loop.
// So the servo runs a faster ACQUISITION gain set until it first converges, then latches into the
// #103 TRACKING gains above. The acquisition gains stay DELAY-ROBUST (loop gain 0.4, stable at 1-2
// sample delay — verified by closed-loop simulation across dt and delay before this code was written).

/// Acquisition proportional gain (ppm/µs): loop gain `k_p·dt = 0.4` at 10 s — WELL-damped for the
/// one-sample-delay plant (`z²−z+0.4` has complex poles at |z|≈0.63, so a fast lightly-ringing
/// transient, NOT the critical `k·dt=0.25`), and still stable at two samples (verified by the delay
/// sweep — 0.6 oscillates at two samples, so 0.4 is the robust operating point; do not raise it or
/// `MAX_LOOP_GAIN_ACQUIRE` without re-running that sweep). 2× the tracking `K_P` (0.02), so the
/// acquisition transient is arrested fast without the deadbeat aggressiveness #103 removed.
pub const K_P_ACQUIRE_PPM_PER_US: f64 = 0.04;

/// Acquisition loop-gain ceiling: caps `k_p_eff·dt ≤ 0.5` at any cadence (the acquisition counterpart
/// of `MAX_LOOP_GAIN`).
pub const MAX_LOOP_GAIN_ACQUIRE: f64 = 0.5;

/// Acquisition integrator rate limit (ppm/s): the integrator may build ~this fast while acquiring, so
/// it reaches a ~50 ppm DC in ~tens of seconds instead of the ~333 s the tracking `I_RATE` (0.15)
/// would take. `K_I` is UNCHANGED — a higher `K_I` overshoots the DC; the rate CAP is the lever.
pub const I_RATE_ACQUIRE_PPM_PER_S: f64 = 5.0;

/// Acquisition output slew-rate limiter (ppm/s): lets `f_phase` track the fast-building integrator
/// during acquisition (the tracking 1.5 ppm/s would itself throttle the catch). Drops back to the
/// tracking limiter once converged.
pub const F_PHASE_SLEW_RATE_ACQUIRE_PPM_PER_S: f64 = 20.0;

/// Convergence latch band (µs): the servo latches from ACQUISITION into TRACKING once `|e|` has been
/// within this band for `CONVERGENCE_SAMPLES` consecutive updates. Just above the deadband so a box
/// that has genuinely settled (not merely dipped through zero mid-drift) enters the damped mode.
pub const CONVERGENCE_BAND_US: i64 = 300;

/// Consecutive in-band samples required to latch convergence (so a single drift-through-zero sample
/// on a still-acquiring high-DC box does not prematurely switch to the slow tracking gains).
pub const CONVERGENCE_SAMPLES: u32 = 3;

/// Re-acquire hysteresis band (µs): a TRACKING servo whose error PERSISTENTLY exceeds this drops
/// BACK to acquisition — a genuine sustained excursion means the held DC is wrong or a disturbance
/// hit, and the fast gains must re-catch it. The band ordering is `PHASE_DEADBAND_US` (200) <
/// `CONVERGENCE_BAND_US` (300) < `REACQUIRE_BAND_US` (1000) < `STEP_BOUNDARY_US` (50 000); a LOCKED
/// box only ever reaches the step path at `> STEP_BOUNDARY_US` (below it it always slews), so on a
/// locked box re-acquisition always pre-empts a slew-path step. (The controller's own adaptive NTP
/// step thresholds — client floor 500 µs, server 200 µs — are reached only when NOT locked, where the
/// step path preserves the DC anyway.)
pub const REACQUIRE_BAND_US: i64 = 1000;

/// Consecutive updates with `|e| > REACQUIRE_BAND_US` required to drop TRACKING back to acquisition.
/// #105 review 🟡: the un-latch MUST be persistent (not a single sample) — a lone >1 ms burst-median
/// outlier on a healthy low-DC box would otherwise instantly restore the acquisition gains and inject
/// an acquisition-sized phase lurch (the exact #103 regression). Requiring 2 consecutive samples
/// keeps a single outlier in the gentle tracking response, and only a genuine sustained excursion
/// re-acquires (after ~one extra NTP interval — negligible next to the DC it is re-catching).
pub const REACQUIRE_SAMPLES: u32 = 2;

// ============================================================================
// PURE HELPERS (feed-forward decoupling + composite word) — no state
// ============================================================================

/// The decoupling carrier line: subtract the commanded phase slew (the `f_phase` that was actually
/// in effect over the just-measured PTP interval) from the raw PTP drift-rate observation, so the
/// PTP frequency servo never treats the deliberate slew as grandmaster disagreement. Sign is `-`
/// because `offset = local - master` (see module docs): a faster local clock grows the observed
/// offset, so a commanded `+f_phase` adds exactly `+f_phase` ppm to `raw_rate`.
pub fn decouple_ptp_rate(raw_rate_ppm: f64, applied_f_phase_ppm: f64) -> f64 {
    raw_rate_ppm - applied_f_phase_ppm
}

/// Compose the one frequency word actually applied to the clock: `f_total = f_ptp + f_phase`,
/// clamped to the servo's own overall frequency bound `max_ppm` (the controller passes
/// `DRIFT_MAX_PPM`) so the platform `adjust_frequency` never receives an out-of-range rate even at
/// the worst-case `f_ptp` + `f_phase` sum.
pub fn compose_frequency(f_ptp_ppm: f64, f_phase_ppm: f64, max_ppm: f64) -> f64 {
    (f_ptp_ppm + f_phase_ppm).clamp(-max_ppm, max_ppm)
}

/// True when a phase error must be STEPPED (cold boot / insane clock), not slewed.
pub fn should_step(e_us: i64) -> bool {
    e_us.abs() > STEP_BOUNDARY_US
}

/// The damping-capped proportional gain for a given `(k_p, ceiling)` pair and update interval
/// (`k_p_eff = min(k_p, ceiling / dt)`, so `k_p_eff·dt ≤ ceiling` at any cadence). The mode-agnostic
/// core used by both the tracking and the #105 acquisition gains.
pub fn effective_kp_with(k_p: f64, max_gain: f64, dt_s: f64) -> f64 {
    if dt_s <= 0.0 {
        return 0.0;
    }
    k_p.min(max_gain / dt_s)
}

/// The effective, damping-capped TRACKING proportional gain for a given update interval (`k_p_eff =
/// min(K_P, MAX_LOOP_GAIN / dt)`). Exposed for testing the stability cap directly.
pub fn effective_kp(dt_s: f64) -> f64 {
    effective_kp_with(K_P_PPM_PER_US, MAX_LOOP_GAIN, dt_s)
}

// ============================================================================
// THE STATEFUL PI SERVO
// ============================================================================

/// One phase-servo update's outputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseSlewOutput {
    /// The commanded phase slew actually applied this update (ppm) — the demand (P + I) after the
    /// #103 output slew-rate limiter and the `±F_PHASE_CAP_PPM` cap. This is the value the caller
    /// composes into `f_ptp` and feeds back to the decoupling, so both stay consistent with the
    /// rate-limited output.
    pub f_phase_ppm: f64,
    /// The proportional contribution (ppm), before the composite cap (0 inside the phase deadband).
    pub p_ppm: f64,
    /// The integrator state after this update (ppm).
    pub i_ppm: f64,
    /// True when the DEMAND (`P + I`) is at `±F_PHASE_CAP_PPM` this update — the servo is asking for
    /// max slew (keyed on the demand, not the rate-limited output, so the alarm arms correctly).
    pub saturated: bool,
    /// True when the "slew saturated" guard has tripped (saturated AND `|e| > 10ms` for ≥60s).
    pub alarm: bool,
    /// #105 — false while the servo is in fast ACQUISITION (still catching the frequency DC), true
    /// once it has latched into damped TRACKING. Surfaced for telemetry / the `[PHASE-SLEW]` log.
    pub converged: bool,
}

/// Bounded PI phase-slew servo. Holds only the integrator state and the saturation dwell timer;
/// everything else is a pure function of `(e, dt)`.
#[derive(Debug, Clone)]
pub struct PhaseSlewServo {
    i_ppm: f64,
    /// #103 — the last commanded `f_phase` (ppm), so the output slew-rate limiter can bound how fast
    /// the frequency word moves between updates. Reset to 0 with the servo (a re-lock starts idle).
    last_f_phase_ppm: f64,
    /// #105 — false = fast ACQUISITION (still catching the frequency DC), true = damped TRACKING.
    /// A fresh servo starts in acquisition; it latches to tracking after `CONVERGENCE_SAMPLES`
    /// consecutive in-band updates and drops back on a `REACQUIRE_BAND_US` excursion.
    converged: bool,
    /// #105 — consecutive in-`CONVERGENCE_BAND_US` updates accumulated toward the convergence latch.
    converged_run: u32,
    /// #105 — consecutive `|e| > REACQUIRE_BAND_US` updates while TRACKING, toward the re-acquire
    /// hysteresis (persistent so a single outlier does not un-latch).
    reacquire_run: u32,
    /// Accumulated seconds during which the slew has been saturated AND `|e| > SAT_ALARM_E_US`.
    saturated_dwell_s: f64,
}

impl Default for PhaseSlewServo {
    fn default() -> Self {
        Self::new()
    }
}

impl PhaseSlewServo {
    pub fn new() -> Self {
        PhaseSlewServo {
            i_ppm: 0.0,
            last_f_phase_ppm: 0.0,
            converged: false,
            converged_run: 0,
            reacquire_run: 0,
            saturated_dwell_s: 0.0,
        }
    }

    /// Current integrator state (ppm).
    pub fn i_ppm(&self) -> f64 {
        self.i_ppm
    }

    /// #105 — true once the servo has latched from fast ACQUISITION into damped TRACKING.
    pub fn converged(&self) -> bool {
        self.converged
    }

    /// #105 — the controller calls this when it STEPS the clock's phase (a large offset the slew
    /// path cannot take) WHILE LOCKED. A step corrects PHASE; the frequency DC the integrator learned
    /// is UNCHANGED, so it is PRESERVED — zeroing it is what caused the high-DC step→reset→runaway
    /// loop (#105). The applied slew stays at the held DC (`last_f_phase_ppm = i_ppm`) so the clock
    /// keeps the frequency across the step, and the servo re-enters fast acquisition to re-verify the
    /// (possibly changed) DC. NOT a full reset — that stays for genuine loss of reference (lock loss /
    /// PTP offline), which `reset_phase_slew` in the controller still does.
    pub fn note_phase_step(&mut self) {
        self.last_f_phase_ppm = self.i_ppm;
        self.converged = false;
        self.converged_run = 0;
        self.reacquire_run = 0;
        self.saturated_dwell_s = 0.0;
    }

    /// One servo update for a fresh median-filtered NTP phase error `e_us` (µs, positive = local
    /// behind UTC) measured `dt_s` seconds after the previous update. Returns the composite
    /// `f_phase` to add to `f_ptp`. Callers MUST only reach this for `|e| ≤ STEP_BOUNDARY_US`
    /// (`should_step` is false); a larger error is a step, not a slew.
    pub fn update(&mut self, e_us: i64, dt_s: f64) -> PhaseSlewOutput {
        let dt = dt_s.max(0.0);
        let e = e_us as f64;

        // #105 — re-acquire hysteresis: a PERSISTENT large excursion while TRACKING means the held DC
        // is wrong (or a disturbance hit), so drop BACK to fast acquisition. Requires REACQUIRE_SAMPLES
        // consecutive out-of-band updates (review 🟡) so a single >1ms burst-median outlier stays in
        // the gentle tracking response instead of triggering an acquisition-sized lurch.
        if self.converged {
            if e_us.abs() > REACQUIRE_BAND_US {
                self.reacquire_run = self.reacquire_run.saturating_add(1);
            } else {
                self.reacquire_run = 0;
            }
            if self.reacquire_run >= REACQUIRE_SAMPLES {
                self.converged = false;
                self.converged_run = 0;
                self.reacquire_run = 0;
            }
        }
        let acquiring = !self.converged;

        // #105 — mode-dependent gains: fast ACQUISITION until the servo first converges, then the
        // #103 damped TRACKING gains (byte-identical to #103, so the low-DC steady state is unchanged).
        let (kp, max_gain, i_rate, f_slew) = if acquiring {
            (
                K_P_ACQUIRE_PPM_PER_US,
                MAX_LOOP_GAIN_ACQUIRE,
                I_RATE_ACQUIRE_PPM_PER_S,
                F_PHASE_SLEW_RATE_ACQUIRE_PPM_PER_S,
            )
        } else {
            (
                K_P_PPM_PER_US,
                MAX_LOOP_GAIN,
                I_RATE_PPM_PER_S,
                F_PHASE_SLEW_RATE_PPM_PER_S,
            )
        };

        // FULL phase deadband: inside it the servo makes no NEW response to the sub-deadband error —
        // the proportional term is zero and the integrator STATE is frozen, so the NTP-path noise
        // floor is never chased. NOTE: the integrator's already-absorbed DC frequency still flows
        // through to `f_phase` below (`target = 0 + self.i_ppm`) — that keeps the clock on-phase; only
        // the *reaction* to the residual is suppressed, the applied slew is NOT forced to zero.
        let in_deadband = e_us.abs() <= PHASE_DEADBAND_US;

        // --- Proportional term (frozen inside the deadband), with the mode's damping-capped gain.
        let p = if in_deadband {
            0.0
        } else {
            (effective_kp_with(kp, max_gain, dt) * e).clamp(-F_PHASE_CAP_PPM, F_PHASE_CAP_PPM)
        };

        // Anti-windup decision uses the tentative composite BEFORE this update's integration.
        let tentative = p + self.i_ppm;
        let would_saturate = tentative.abs() > F_PHASE_CAP_PPM;

        // --- Integrator: frozen inside the deadband (so measurement jitter cannot random-walk the
        // frequency); rate-limited to ≤ the mode's I-rate; anti-windup (never integrate further INTO
        // saturation); hard-clamped to ±I_CLAMP_PPM. `K_I` is the SAME in both modes — acquisition
        // speed comes from the higher rate CAP, not a higher `K_I` (which would overshoot the DC).
        if !in_deadband {
            let unbounded = K_I_PPM_PER_US_S * e * dt;
            let rate_cap = i_rate * dt;
            let delta = unbounded.clamp(-rate_cap, rate_cap);
            let deepens_saturation = would_saturate && ((delta > 0.0) == (tentative > 0.0));
            if !deepens_saturation {
                self.i_ppm = (self.i_ppm + delta).clamp(-I_CLAMP_PPM, I_CLAMP_PPM);
            }
        }

        // --- The DEMAND (target) composite phase slew, capped. Saturation and the alarm key on the
        // DEMAND (the servo asks for max slew and still cannot converge), independent of the output
        // rate-limiter's ramp below — so the alarm semantics survive the limiter unchanged.
        let target = (p + self.i_ppm).clamp(-F_PHASE_CAP_PPM, F_PHASE_CAP_PPM);
        let saturated = target.abs() >= F_PHASE_CAP_PPM - 1e-9;

        // --- Output slew-rate limiter (the mode's cap): the commanded f_phase moves by at most
        // `f_slew · dt` toward the demand this update, bounding clock acceleration. Re-clamped to the
        // hard cap for safety.
        let max_step = f_slew * dt;
        let f_phase = (self.last_f_phase_ppm
            + (target - self.last_f_phase_ppm).clamp(-max_step, max_step))
        .clamp(-F_PHASE_CAP_PPM, F_PHASE_CAP_PPM);
        self.last_f_phase_ppm = f_phase;

        // #105 — convergence latch: after CONVERGENCE_SAMPLES consecutive updates with |e| inside the
        // convergence band, latch from ACQUISITION into TRACKING (stays latched until a re-acquire
        // excursion above, a step, or a full reset). A single drift-through-zero sample on a still-
        // acquiring high-DC box resets the run, so it never latches prematurely.
        if !self.converged {
            if e_us.abs() <= CONVERGENCE_BAND_US {
                self.converged_run = self.converged_run.saturating_add(1);
            } else {
                self.converged_run = 0;
            }
            if self.converged_run >= CONVERGENCE_SAMPLES {
                self.converged = true;
                self.reacquire_run = 0;
            }
        }

        // --- "Slew saturated" guard: dwell accumulates only while the DEMAND is capped AND the error
        // is genuinely large; any recovery below either threshold resets it.
        if saturated && e_us.abs() > SAT_ALARM_E_US {
            self.saturated_dwell_s += dt;
        } else {
            self.saturated_dwell_s = 0.0;
        }
        let alarm = self.saturated_dwell_s >= SAT_ALARM_DWELL_S;

        PhaseSlewOutput {
            f_phase_ppm: f_phase,
            p_ppm: p,
            i_ppm: self.i_ppm,
            saturated,
            alarm,
            converged: self.converged,
        }
    }

    /// #97 guard — emit the loud, grep-able alarm line ONCE per trip edge. The caller decides when
    /// (it holds the rate-limit / edge state); this is the message text in one place.
    pub fn log_saturated_alarm(e_us: i64, f_phase_ppm: f64) {
        warn!(
            "[PHASE-SLEW][SATURATED] slew capped at {:+.0}ppm while |e|={}us exceeds {}us for \
             >={}s — the phase servo cannot keep up (upstream way off, or a genuine large offset \
             the step path should have taken); UTC phase is NOT converging",
            f_phase_ppm, e_us, SAT_ALARM_E_US, SAT_ALARM_DWELL_S as i64
        );
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn decoupling_subtracts_the_commanded_slew_from_the_raw_rate() {
        // The PTP servo observed +7 ppm of drift, of which +5 ppm is our own commanded slew.
        // It must only respond to the residual +2 ppm genuine grandmaster disagreement.
        assert!((decouple_ptp_rate(7.0, 5.0) - 2.0).abs() < 1e-9);
        // A negative commanded slew adds negative rate; decoupling adds it back.
        assert!((decouple_ptp_rate(-3.0, -5.0) - 2.0).abs() < 1e-9);
        // Zero slew (flag off / servo idle) leaves the PTP rate untouched — the pre-#97 behaviour.
        assert!((decouple_ptp_rate(4.2, 0.0) - 4.2).abs() < 1e-9);
    }

    #[test]
    fn composite_frequency_sums_and_clamps_to_the_overall_bound() {
        assert!((compose_frequency(30.0, 150.0, 500.0) - 180.0).abs() < 1e-9);
        // f_ptp + f_phase past the bound is clamped (worst case ±500 ptp + ±200 phase).
        assert!((compose_frequency(450.0, 200.0, 500.0) - 500.0).abs() < 1e-9);
        assert!((compose_frequency(-450.0, -200.0, 500.0) + 500.0).abs() < 1e-9);
    }

    #[test]
    fn should_step_boundary_is_50ms_both_signs() {
        assert!(!should_step(49_999));
        assert!(!should_step(-49_999));
        assert!(!should_step(50_000)); // exactly at the boundary still slews (strict `>`)
        assert!(should_step(50_001));
        assert!(should_step(-50_001));
    }

    #[test]
    fn effective_kp_is_damping_capped_but_full_design_gain_at_nominal_cadence() {
        // At the ≤12.5 s master cadence the exact design k_p (0.02) is preserved.
        assert!((effective_kp(10.0) - 0.02).abs() < 1e-9);
        assert!((effective_kp(5.0) - 0.02).abs() < 1e-9); // faster still ⇒ full design gain
                                                          // Loop gain at 10 s is the overdamped 0.2, never the old deadbeat 1.0.
        assert!((effective_kp(10.0) * 10.0 - 0.2).abs() < 1e-9);
        // At the client's slow 30 s cadence the ceiling binds so k_p_eff·dt = 0.25 (critically
        // damped) instead of the old unstable-with-delay deadbeat 1.0.
        assert!((effective_kp(30.0) - (0.25 / 30.0)).abs() < 1e-9);
        assert!((effective_kp(30.0) * 30.0 - 0.25).abs() < 1e-9);
        // The ceiling holds at ANY cadence: k_p_eff·dt ≤ MAX_LOOP_GAIN.
        for &dt in &[1.0, 8.0, 12.5, 20.0, 60.0, 120.0] {
            assert!(
                effective_kp(dt) * dt <= 0.25 + 1e-9,
                "loop gain exceeded 0.25 at dt={}",
                dt
            );
        }
    }

    // ---- P term -----------------------------------------------------------

    /// A servo latched into TRACKING (fed in-band updates until it converges; the integrator stays 0).
    fn converged_tracking_servo() -> PhaseSlewServo {
        let mut s = PhaseSlewServo::new();
        for _ in 0..(CONVERGENCE_SAMPLES + 1) {
            s.update(0, 10.0); // |e|=0 is inside the convergence band ⇒ latches to tracking; i stays 0
        }
        assert!(s.converged(), "helper must reach tracking mode");
        s
    }

    #[test]
    fn proportional_term_uses_the_mode_gain_and_tracks_sign() {
        // ACQUISITION (a fresh servo): the faster P gain 0.04 ⇒ P = 0.04*1000 = 40ppm.
        let mut acq = PhaseSlewServo::new();
        let out = acq.update(1000, 10.0);
        assert!(
            (out.p_ppm - 40.0).abs() < 1.0,
            "acquisition P should be ~+40ppm for +1ms error, got {}",
            out.p_ppm
        );
        assert!(out.f_phase_ppm > 0.0, "positive error ⇒ positive slew");
        // TRACKING (converged): the damped P gain 0.02 ⇒ P = 0.02*900 = 18ppm. 900us stays below the
        // 1000us re-acquire band, so the servo does NOT drop back to acquisition.
        let mut trk = converged_tracking_servo();
        let out = trk.update(900, 10.0);
        assert!(
            (out.p_ppm - 18.0).abs() < 1.0,
            "tracking P should be ~+18ppm for +900us error, got {}",
            out.p_ppm
        );
    }

    #[test]
    fn proportional_term_flips_sign_with_error() {
        let mut s = PhaseSlewServo::new();
        let out = s.update(-1000, 10.0);
        assert!(out.p_ppm < 0.0 && out.f_phase_ppm < 0.0);
    }

    // ---- caps / anti-windup ----------------------------------------------

    #[test]
    fn tracking_output_slew_rate_limits_the_f_phase_ramp() {
        // A converged servo with a 900us error (tracking): the demand is ~18ppm, but the #103 output
        // limiter moves f_phase by at most 1.5ppm/s = 15ppm/step — so the first step ramps to ≤15, not
        // straight to the ~18ppm demand.
        let mut trk = converged_tracking_servo();
        let out = trk.update(900, 10.0);
        assert!(
            out.f_phase_ppm > 0.0 && out.f_phase_ppm <= 15.0 + 1e-6,
            "tracking f_phase must ramp ≤15ppm/step, got {}",
            out.f_phase_ppm
        );
    }

    #[test]
    fn demand_saturates_and_the_output_never_exceeds_the_200ppm_cap() {
        // A huge (but < 50ms step boundary) error saturates the demand; the applied slew must never
        // exceed the ±200ppm cap. (A fresh servo is in acquisition, whose faster output limiter
        // reaches the cap quickly — the point here is that the cap HOLDS.)
        let mut s = PhaseSlewServo::new();
        let first = s.update(40_000, 10.0);
        assert!(
            first.saturated,
            "the demand (P+I) is at the cap immediately"
        );
        let mut last = first;
        for _ in 0..40 {
            last = s.update(40_000, 10.0);
            assert!(
                last.f_phase_ppm <= 200.0 + 1e-6,
                "f_phase exceeded the cap: {}",
                last.f_phase_ppm
            );
        }
        assert!(
            (last.f_phase_ppm - 200.0).abs() < 1e-6,
            "reaches the cap, got {}",
            last.f_phase_ppm
        );
    }

    #[test]
    fn integrator_rate_limit_is_per_mode() {
        // TRACKING: I moves ≤ 0.15ppm/s = 1.5ppm/step (a converged servo + a 900us error that stays
        // below the re-acquire band).
        let mut trk = converged_tracking_servo();
        let before = trk.i_ppm();
        let out = trk.update(900, 10.0);
        assert!(
            (out.i_ppm - before).abs() <= 1.5 + 1e-6,
            "tracking |ΔI|={} must be ≤ 1.5ppm",
            (out.i_ppm - before).abs()
        );
        // ACQUISITION: I may move up to the faster cap 5ppm/s = 50ppm/step — and DOES move faster than
        // the tracking cap (that is the whole point of the mode split).
        let mut acq = PhaseSlewServo::new();
        let out = acq.update(5_000, 10.0);
        assert!(
            out.i_ppm.abs() <= 50.0 + 1e-6,
            "acquisition |ΔI|={} must be ≤ the 50ppm/step cap",
            out.i_ppm
        );
        assert!(
            out.i_ppm.abs() > 1.5,
            "acquisition must build I faster than the 1.5ppm tracking cap, got {}",
            out.i_ppm
        );
    }

    #[test]
    fn integrator_and_slew_stay_within_their_hard_bounds_under_adversarial_drive() {
        let mut s = PhaseSlewServo::new();
        for _ in 0..10_000 {
            let out = s.update(45_000, 10.0); // just under the step boundary, sustained max
            assert!(
                out.i_ppm.abs() <= I_CLAMP_PPM + 1e-6,
                "I clamp ±250 violated: {}",
                out.i_ppm
            );
            assert!(
                out.f_phase_ppm.abs() <= F_PHASE_CAP_PPM + 1e-6,
                "f_phase cap ±200 violated: {}",
                out.f_phase_ppm
            );
        }
    }

    #[test]
    fn anti_windup_freezes_the_integrator_while_the_slew_is_saturated() {
        let mut s = PhaseSlewServo::new();
        // Drive to saturation for a while.
        let mut last_i = 0.0;
        for _ in 0..50 {
            last_i = s.update(45_000, 10.0).i_ppm;
        }
        // Once saturated, another same-sign over-cap update must NOT keep growing the integrator.
        let out = s.update(45_000, 10.0);
        assert!(out.saturated);
        assert!(
            (out.i_ppm - last_i).abs() < 1e-6,
            "anti-windup: I grew from {} to {} while saturated",
            last_i,
            out.i_ppm
        );
    }

    #[test]
    fn nothing_moves_inside_the_deadband_so_jitter_never_walks_the_frequency() {
        let mut s = PhaseSlewServo::new();
        // Alternating ±100us jitter, all inside the 200us FULL deadband, on a FRESH servo (i starts
        // 0): the integrator never engages (stays 0) and no proportional kick builds, so a servo that
        // has only ever seen sub-deadband jitter injects nothing. (Once the integrator HAS absorbed a
        // DC, that held frequency keeps flowing — see `deadband_holds_the_converged_dc_frequency…`.)
        for k in 0..200 {
            let e = if k % 2 == 0 { 100 } else { -100 };
            let out = s.update(e, 30.0);
            assert!(
                out.i_ppm.abs() < 1e-9,
                "integrator walked on in-deadband jitter: {}",
                out.i_ppm
            );
            assert!(
                out.p_ppm.abs() < 1e-9,
                "P applied inside the deadband: {}",
                out.p_ppm
            );
            assert!(
                out.f_phase_ppm.abs() < 1e-9,
                "phase correction inside the deadband: {}",
                out.f_phase_ppm
            );
        }
    }

    // ---- convergence (closed loop) ---------------------------------------

    /// Drive the servo in a closed loop against a constant DC inflow (the master's Dante-vs-UTC
    /// error), with ZERO transport delay: each interval `e` accrues `+inflow·dt`, then the slew
    /// removes `f_phase·dt`. (#103 convergence is deliberately slow — minutes; the delayed-plant ring
    /// tests below add the transport delay the damping targets.)
    fn run_closed_loop(inflow_ppm: f64, dt_s: f64, e0_us: f64, steps: usize) -> (f64, f64, f64) {
        let mut s = PhaseSlewServo::new();
        let mut e = e0_us;
        let mut steady_peak = 0.0_f64;
        for n in 0..steps {
            let out = s.update(e.round() as i64, dt_s);
            e += (inflow_ppm - out.f_phase_ppm) * dt_s;
            if n > steps / 2 {
                steady_peak = steady_peak.max(e.abs());
            }
        }
        (e, s.i_ppm(), steady_peak)
    }

    #[test]
    fn converges_into_the_deadband_on_a_23ppm_dc_inflow_and_the_integrator_absorbs_it() {
        // The master case: constant 23 ppm error, starting 5 ms off. Deliberately SLOW (minutes) —
        // give it 300 updates (50 min at 10 s). It parks within the phase deadband and the integrator
        // absorbs ~the whole 23 ppm DC (that is what holds UTC phase without steps).
        let (e, i, peak) = run_closed_loop(23.0, 10.0, 5000.0, 300);
        assert!(
            e.abs() < 300.0,
            "final |e|={}us must settle inside the deadband band",
            e.abs()
        );
        assert!(
            peak < 400.0,
            "steady-state peak |e|={}us must be small, no overshoot",
            peak
        );
        assert!(
            (i - 23.0).abs() < 6.0,
            "integrator must absorb ~the 23ppm DC inflow, got {}ppm",
            i
        );
    }

    #[test]
    fn converges_from_5ms_within_minutes_not_seconds() {
        // #103: convergence is deliberately slow (the rig needs stability, not speed). Prove it
        // reaches the deadband within a few minutes from a 5 ms error at the 10 s cadence — NOT the
        // old deadbeat 40 s (which is what rang).
        let mut s = PhaseSlewServo::new();
        let mut e = 5000.0_f64;
        let mut converged_at: Option<f64> = None;
        for n in 0..90 {
            let out = s.update(e.round() as i64, 10.0);
            e += (23.0 - out.f_phase_ppm) * 10.0;
            if converged_at.is_none() && e.abs() < 300.0 {
                converged_at = Some((n as f64 + 1.0) * 10.0);
            }
        }
        let t = converged_at.expect("must converge into the deadband");
        assert!(
            t <= 600.0,
            "converged in {}s, expected within a few minutes",
            t
        );
    }

    #[test]
    fn worst_case_66ppm_master_still_converges_into_the_deadband() {
        // 66 ppm = the worst-ever measured Dante-GM rate error (#83). A bigger DC takes the slow
        // integrator longer to absorb, so give it more updates; it still parks inside the band.
        let (e, i, peak) = run_closed_loop(66.0, 10.0, 0.0, 400);
        assert!(
            e.abs() < 300.0 && peak < 400.0,
            "66ppm: final={}us peak={}us",
            e,
            peak
        );
        assert!(
            (i - 66.0).abs() < 8.0,
            "integrator absorbs the 66ppm DC, got {}ppm",
            i
        );
    }

    #[test]
    fn damping_cap_keeps_the_client_30s_cadence_stable_not_a_divergent_limit_cycle() {
        // Without a gain cap, k_p·dt = 0.6 at 30 s; the old deadbeat cap forced k_p·dt = 1.0 (marginal
        // with delay). The #103 damping ceiling holds k_p_eff·dt = 0.25 (critically damped), keeping
        // the 30 s-cadence loop bounded and quiet even in this zero-delay closed loop.
        let (_e, _i, peak) = run_closed_loop(23.0, 30.0, 5000.0, 200);
        assert!(
            peak < 600.0,
            "damping cap must keep the 30s-cadence loop bounded; steady peak was {}us",
            peak
        );
    }

    // ---- saturation alarm guard ------------------------------------------

    #[test]
    fn saturation_alarm_raises_only_after_60s_of_sustained_capped_large_error() {
        let mut s = PhaseSlewServo::new();
        // |e| = 20ms (> 10ms alarm threshold, < 50ms step boundary), dt = 10 s.
        // 5 updates × 10 s = 50 s < 60 s ⇒ no alarm yet.
        let mut out = s.update(20_000, 10.0);
        for _ in 0..4 {
            out = s.update(20_000, 10.0);
            assert!(out.saturated);
        }
        assert!(!out.alarm, "no alarm before 60s of dwell");
        // The 6th (60 s total) trips it.
        out = s.update(20_000, 10.0);
        assert!(
            out.alarm,
            "alarm must raise after ≥60s saturated with |e|>10ms"
        );
    }

    #[test]
    fn saturation_dwell_resets_when_the_error_recovers() {
        let mut s = PhaseSlewServo::new();
        for _ in 0..5 {
            s.update(20_000, 10.0); // 50 s of dwell accrued
        }
        // Error recovers below the alarm threshold: dwell resets, no alarm.
        let out = s.update(500, 10.0);
        assert!(!out.alarm);
        // And a fresh large error must again take a full 60 s to re-arm.
        let mut out2 = s.update(20_000, 10.0);
        for _ in 0..4 {
            out2 = s.update(20_000, 10.0);
        }
        assert!(
            !out2.alarm,
            "dwell must have reset — 50s is not yet an alarm"
        );
    }

    // ---- #103: damping under a realistic loop delay --------------------------
    //
    // The #97 convergence tests above drive a ZERO-DELAY plant, where a deadbeat servo
    // (`k_p·dt = 1`) is genuinely stable — which is why they never caught the production
    // oscillation. The REAL loop has ≈1 sample of transport delay (the burst-median NTP measurement
    // reflects a lagged clock state; the composite PTP+phase word and the integrator pole add more).
    // These tests add that one missing sample of delay and assert the servo does not ring.

    /// Closed loop with ONE sample of transport delay: the f_phase commanded at step n only affects
    /// the plant at step n+1 (`e_{n+1} = e_n + (inflow − f_phase_{n-1})·dt`). Returns the
    /// steady-state peak |e| and the worst rolling 6-sample spread (the exact metric camera-box's
    /// E2E clock gate bounds at 2000 µs).
    fn closed_loop_one_sample_delay(
        inflow_ppm: f64,
        dt_s: f64,
        e0_us: f64,
        steps: usize,
    ) -> (f64, f64) {
        let mut s = PhaseSlewServo::new();
        let mut e = e0_us;
        let mut f_delayed = 0.0_f64;
        let mut window: Vec<f64> = Vec::new();
        let mut steady_peak = 0.0_f64;
        let mut worst_spread6 = 0.0_f64;
        let warmup = steps / 2;
        for n in 0..steps {
            let out = s.update(e.round() as i64, dt_s);
            e += (inflow_ppm - f_delayed) * dt_s; // plant integrates LAST step's command (the delay)
            f_delayed = out.f_phase_ppm;
            window.push(e);
            if window.len() > 6 {
                window.remove(0);
            }
            if n > warmup {
                steady_peak = steady_peak.max(e.abs());
                let mx = window.iter().cloned().fold(f64::MIN, f64::max);
                let mn = window.iter().cloned().fold(f64::MAX, f64::min);
                worst_spread6 = worst_spread6.max(mx - mn);
            }
        }
        (steady_peak, worst_spread6)
    }

    #[test]
    fn damped_servo_does_not_ring_on_a_one_sample_delay_plant() {
        // 23 ppm Dante-vs-UTC DC, 10 s master cadence, starting mid-swing like the live +455..+568us.
        let (peak, spread6) = closed_loop_one_sample_delay(23.0, 10.0, 500.0, 400);
        // A deadbeat servo rings to a ~4000us sustained 6-sample spread here (the prod incident, which
        // fails camera-box's 2000us clock gate); a damped one settles inside the phase deadband.
        assert!(
            spread6 < 500.0,
            "6-sample spread {}us must be well under the 2000us gate (a deadbeat servo rings to ~4ms)",
            spread6
        );
        assert!(
            peak < 400.0,
            "steady peak |e| {}us must converge, not sustain a limit cycle",
            peak
        );
    }

    #[test]
    fn damped_servo_makes_only_smooth_frequency_changes() {
        // Track the largest steady-state f_phase step. The live incident swung f_phase
        // +100.81 -> +15.60 -> +8.00 -> +35.19 ppm (~85 ppm/step); a damped servo moves it a few ppm.
        let mut s = PhaseSlewServo::new();
        let mut e = 500.0_f64;
        let mut f_delayed = 0.0_f64;
        let mut last_f = 0.0_f64;
        let mut max_df = 0.0_f64;
        for n in 0..400 {
            let out = s.update(e.round() as i64, 10.0);
            e += (23.0 - f_delayed) * 10.0;
            f_delayed = out.f_phase_ppm;
            if n > 200 {
                max_df = max_df.max((out.f_phase_ppm - last_f).abs());
            }
            last_f = out.f_phase_ppm;
        }
        // Assert BELOW the output rate-limiter's own bound (F_PHASE_SLEW_RATE·dt = 15 ppm/step), so
        // this genuinely tests DAMPING, not merely that the limiter exists: a badly-damped servo
        // with the same limiter would ride at ~15 ppm/step, a damped one moves only a few ppm.
        assert!(
            max_df < 8.0,
            "steady-state |Δf_phase/step| {}ppm must stay smooth — well under the 15ppm limiter bound (a deadbeat servo swings 100+ppm)",
            max_df
        );
    }

    #[test]
    fn fresh_servo_builds_no_correction_from_sub_deadband_error() {
        // A steady 150us error is inside the ≈200us full deadband (the NTP-path noise floor). On a
        // FRESH servo the integrator never engages, so no correction builds at all — no P kick, no
        // accumulated DC. (A deadbeat servo has no P deadband: it applies P = 0.1·150 = 15ppm and
        // keeps nudging.) The CONVERGED case — where the integrator has already absorbed the DC and
        // that held frequency must KEEP being applied inside the band — is the next test.
        let mut s = PhaseSlewServo::new();
        let mut out = s.update(150, 10.0);
        for _ in 0..50 {
            out = s.update(150, 10.0);
        }
        assert!(
            out.p_ppm.abs() < 1e-9,
            "proportional term must be frozen inside the deadband, got {}ppm",
            out.p_ppm
        );
        assert!(
            out.f_phase_ppm.abs() < 1e-9,
            "a fresh servo builds no correction inside the deadband, got {}ppm",
            out.f_phase_ppm
        );
    }

    #[test]
    fn deadband_holds_the_converged_dc_frequency_it_does_not_drop_to_zero() {
        // #103 review 🟡: inside the deadband P and the integrator STATE freeze, but the integrator's
        // already-absorbed DC frequency must KEEP being applied — that held frequency is exactly what
        // holds the clock on-phase. Zeroing f_phase in-band would let the ~23ppm Dante-vs-UTC drift
        // repop (the #103 failure), and the fresh-servo tests above (i=0) could NOT catch that. So:
        // drive the servo to convergence first, THEN feed an in-deadband error and prove the held DC
        // is still commanded.
        let mut s = PhaseSlewServo::new();
        // Converge against a 23 ppm DC from a 3 ms error (zero-delay closed loop) ⇒ I ≈ 23 ppm.
        let mut e = 3000.0_f64;
        for _ in 0..300 {
            let out = s.update(e.round() as i64, 10.0);
            e += (23.0 - out.f_phase_ppm) * 10.0;
        }
        assert!(
            (s.i_ppm() - 23.0).abs() < 6.0,
            "precondition: integrator must have absorbed the ~23ppm DC, got {}ppm",
            s.i_ppm()
        );
        // Now a small in-deadband error: P is frozen (0), but the held DC frequency is still applied.
        let out = s.update(120, 10.0);
        assert!(
            out.p_ppm.abs() < 1e-9,
            "P must be frozen inside the deadband, got {}ppm",
            out.p_ppm
        );
        assert!(
            (out.f_phase_ppm - s.i_ppm()).abs() < 1e-9 && out.f_phase_ppm > 15.0,
            "the held DC frequency (~23ppm) must KEEP being applied inside the deadband, got {}ppm",
            out.f_phase_ppm
        );
    }

    // ---- #105: dual-mode acquisition + step preserves the DC ----------------
    //
    // v1.8.50 (the #103 damping) holds a low-DC box but on a high-DC box (~50 ppm, e.g. cam1) the
    // slow integrator (I_RATE 0.15 ppm/s) cannot build the holding frequency, and a step zeroes it —
    // the offset runs away and the box loops step→reset→step. These tests add a high-DC plant and the
    // step interaction; they FAIL on 1.8.50 and pass on the 1.8.51 dual-mode servo.

    /// Fresh servo on a constant `inflow` ppm DC, one-sample loop delay, NO steps. Returns the peak
    /// |e| reached and the time |e| first settled within the deadband (or None). A high-DC box must
    /// converge FAST and keep the peak below the box's small adaptive step threshold, or a step fires
    /// and (pre-#105) resets the servo into a runaway loop.
    fn acquisition_peak_and_settle(inflow_ppm: f64, dt_s: f64, steps: usize) -> (f64, Option<f64>) {
        let mut s = PhaseSlewServo::new();
        let mut e = 0.0_f64;
        let mut f_delayed = 0.0_f64;
        let mut peak = 0.0_f64;
        let mut settled: Option<f64> = None;
        for n in 0..steps {
            let out = s.update(e.round() as i64, dt_s);
            e += (inflow_ppm - f_delayed) * dt_s;
            f_delayed = out.f_phase_ppm;
            peak = peak.max(e.abs());
            if settled.is_none() && n > 3 && e.abs() <= PHASE_DEADBAND_US as f64 {
                settled = Some((n as f64 + 1.0) * dt_s);
            }
        }
        (peak, settled)
    }

    #[test]
    fn high_dc_50ppm_acquisition_stays_below_the_step_threshold_and_settles_fast() {
        // 50 ppm Dante-vs-UTC DC (cam1 class). The acquisition transient must stay well below the box's
        // small adaptive step threshold (~1.6 ms per the #105 journal) so no step ever fires, and settle
        // into the deadband within ~2 min. v1.8.50's slow integrator parks e ~2.5 ms (> threshold → the
        // runaway loop) and takes many minutes.
        let (peak, settled) = acquisition_peak_and_settle(50.0, 10.0, 400);
        assert!(
            peak < 1600.0,
            "acquisition peak |e|={}us must stay below the ~1.6ms step threshold (no step fires)",
            peak
        );
        let t = settled.expect("must settle into the deadband");
        assert!(t <= 150.0, "must settle within ~2min, took {}s", t);
    }

    #[test]
    fn note_phase_step_preserves_the_learned_frequency_dc() {
        // Converge against a 50 ppm DC so the integrator holds ~50 ppm, then a STEP happens (a large
        // phase error the slew path cannot take). The step corrects PHASE; the learned FREQUENCY DC
        // must SURVIVE — zeroing it re-starts the runaway (the #105 loop). v1.8.50 / the RED stub
        // zero the integrator here.
        let mut s = PhaseSlewServo::new();
        let mut e = 0.0_f64;
        let mut f_delayed = 0.0_f64;
        for _ in 0..200 {
            let out = s.update(e.round() as i64, 10.0);
            e += (50.0 - f_delayed) * 10.0;
            f_delayed = out.f_phase_ppm;
        }
        let dc_before = s.i_ppm();
        assert!(
            dc_before > 40.0,
            "precondition: integrator must hold ~50ppm before the step, got {}ppm",
            dc_before
        );
        s.note_phase_step();
        assert!(
            (s.i_ppm() - dc_before).abs() < 1.0,
            "the learned frequency DC must be PRESERVED across a phase step, was {}ppm now {}ppm",
            dc_before,
            s.i_ppm()
        );
    }

    #[test]
    fn a_single_outlier_does_not_un_latch_tracking_but_a_sustained_excursion_does() {
        // #105 review 🟡: a lone >1ms burst-median outlier on a healthy TRACKING box must NOT restore
        // the acquisition gains (that would inject an acquisition-sized phase lurch — the #103
        // regression). It stays in the gentle tracking response; only a SUSTAINED excursion re-acquires.
        let mut s = converged_tracking_servo();
        assert!(s.converged());
        // one outlier at 2000us (> REACQUIRE_BAND 1000): stays TRACKING, gentle output (P 0.02, slew 1.5).
        let out = s.update(2000, 10.0);
        assert!(s.converged(), "a single outlier must NOT un-latch tracking");
        assert!(
            out.f_phase_ppm.abs() <= 15.0 + 1e-6,
            "the single-outlier response must be the gentle tracking ramp (≤15ppm/step), got {}",
            out.f_phase_ppm
        );
        // a back-in-band sample forgets the outlier; still tracking.
        s.update(0, 10.0);
        assert!(s.converged(), "still tracking after the outlier passes");
        // a SUSTAINED excursion (REACQUIRE_SAMPLES consecutive out-of-band) DOES drop to acquisition.
        let mut converged = true;
        for _ in 0..REACQUIRE_SAMPLES {
            converged = s.update(2000, 10.0).converged;
        }
        assert!(
            !converged,
            "a sustained excursion (≥REACQUIRE_SAMPLES) must re-acquire"
        );
    }
}
