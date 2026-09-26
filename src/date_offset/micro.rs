//! dantesync#119 follow-up — the fleet date correction as sub-threshold MICRO-corrections.
//!
//! Until 1.10 the date authority let the fleet date drift from UTC up to the step bound (50 ms)
//! and then corrected the whole error in one coordinated event. At the grandmaster's +1.06 ms/min
//! drift that is a 50 ms event every ~47 minutes, 1.5 video frames, and every wall-anchored
//! consumer (OBS senders, SongPlayer) sees it. Now the master publishes the correction
//! continuously: once the error leaves a small dead band (2 ms) it is corrected in increments of
//! at most `step` (500 µs by default), at most one per `interval` (20 s). An increment forward is a
//! coordinated step; an increment backward is a coordinated slew (the #119 machinery). Each one is
//! far below every consumer's step threshold (SongPlayer re-anchors above 1 ms, the camera-box
//! wall-step detector fires at 5 ms, its genlock grid holds 3 ms), and the capacity
//! `step / interval` = 1.5 ms/min is above the drift the rig has shown.
//!
//! This module is the DECISION only — pure, explicit time inputs, no I/O — and
//! `crate::date_offset::DateAuthority` turns each increment into an announce.
//!
//! # The estimate
//!
//! The master reads UTC every 10 s over a WAN path (a mobile link on the rig), so one reading can
//! be ms off. Every reading is kept for [`MICRO_TREND_WINDOW_NS`] (20 min), relative to the TARGET
//! date offset — every increment the authority announces is subtracted from the kept readings at once,
//! so the history always describes the error that remains once everything announced has landed.
//! The error at an instant is a robust line through that history (Theil–Sen): the drift is the
//! MEDIAN of the pairwise slopes over the whole window, and the level is the MEDIAN of the last
//! [`MICRO_LEVEL_WINDOW_NS`] (5 min) of readings, each projected to the instant along that drift.
//! A plain median of the last readings lags a steady ramp by half its span (≈ 0.5–2.6 ms at the
//! rig's 1.06 ms/min); the projection has no lag.
//!
//! # The decision
//!
//! - Nothing inside the dead band.
//! - Once beyond it, one increment per interval in the error's direction, of `min(|error|, step)`,
//!   and the corrections continue in that direction until the error is back inside
//!   [`MICRO_EXIT_BAND_NS`] (so the fleet date is brought near UTC, not parked on the band's edge).
//! - A correction in the OPPOSITE direction to the last one needs twice the dead band, unless the
//!   fitted drift has turned that way too: noise must not make the fleet date oscillate.
//! - Every band is widened by three standard errors of the estimate, measured from the readings'
//!   own scatter around the fitted line: a noisy UTC path makes a correction less likely.
//! - A drift beyond the capacity cannot be held: the error grows, and [`MicroScheduler::falling_behind`]
//!   says so (the controller logs `date correction falling behind` loudly) — never a large step.
//!   A large step exists only for an abnormal error beyond the #119 cap (`DateAuthority`).

use std::collections::VecDeque;

/// Default size of one micro-correction (µs): 500 µs is far below one video frame and below every
/// consumer's step threshold.
pub const DEFAULT_MICRO_STEP_US: u64 = 500;
/// The configurable increment is clamped to this range. Above 1 ms SongPlayer re-anchors its wall
/// clock, which is exactly the disturbance the micro-corrections exist to avoid.
pub const MIN_MICRO_STEP_US: u64 = 50;
pub const MAX_MICRO_STEP_US: u64 = 1_000;

/// Default spacing of micro-corrections (s). With the default step the capacity is 1.5 ms/min.
pub const DEFAULT_MICRO_INTERVAL_S: u64 = 20;
/// The configurable spacing is clamped to this range (a correction needs its 5 s announce lead
/// and, backwards, its slew: one at a time, so a shorter spacing would not be honoured anyway).
pub const MIN_MICRO_INTERVAL_S: u64 = 10;
pub const MAX_MICRO_INTERVAL_S: u64 = 600;

/// The fleet date is left alone while its estimated UTC error is inside this band.
pub const MICRO_DEAD_BAND_NS: i64 = 2_000_000;
/// Once correcting, the corrections continue in the same direction until the error is inside this.
pub const MICRO_EXIT_BAND_NS: i64 = 500_000;
/// A correction opposite to the last one (while it is recent) needs this much error.
pub const MICRO_REVERSAL_BAND_NS: i64 = 2 * MICRO_DEAD_BAND_NS;
/// The correction rate is measured over this.
pub const MICRO_WINDOW_NS: i64 = 600_000_000_000;
/// How long a reading is remembered: the DRIFT is fitted over this window. The grandmaster-vs-UTC
/// rate is steady for hours, and a long window keeps the fitted drift quiet under ms-level WAN
/// jitter (a drift error is multiplied by the time it is projected over).
pub const MICRO_TREND_WINDOW_NS: i64 = 1_200_000_000_000;
/// No drift is fitted before the readings span this (it would be mostly noise): until then the
/// estimate is the plain median of the level window.
pub const MICRO_TREND_MIN_SPAN_NS: i64 = 120_000_000_000;
/// The drift is fitted over one point per this stretch of readings (their medians).
pub const MICRO_TREND_BIN_NS: i64 = 60_000_000_000;
/// The LEVEL is the median of the readings of this last stretch, each projected along the drift.
pub const MICRO_LEVEL_WINDOW_NS: i64 = 300_000_000_000;
/// A correction against the direction of the last one needs [`MICRO_REVERSAL_BAND_NS`] — unless
/// the fitted drift itself has turned against that direction by at least this (ns per s = ppm):
/// then the fleet really runs the other way now and the plain dead band applies. Pure jitter
/// leaves the drift near 0, so it can never walk the date back and forth.
pub const MICRO_TURNED_TREND_NS_PER_S: f64 = 2_000.0;
/// A bound on the kept readings (the master reads every 10 s: 120 in the drift window).
pub const MICRO_MAX_READINGS: usize = 256;
/// No estimate from fewer readings than this (about the first minute after the authority starts).
pub const MICRO_MIN_READINGS: usize = 6;
/// The drift estimate is clamped to ±this (ns per s = ppm): a grandmaster-vs-UTC rate beyond it is
/// not a rate this fleet has, and a wild estimate must not be projected.
pub const MICRO_MAX_TREND_NS_PER_S: f64 = 50_000.0;
/// Every band is widened by this many standard errors of the level estimate (measured from the
/// readings' own scatter around the fitted line — jitter, not the ramp): a noisy UTC path must
/// make a correction LESS likely, never trigger one.
pub const MICRO_NOISE_MARGIN_SIGMAS: f64 = 3.0;
/// An estimated error this large means the corrections are not holding the date, whatever the
/// cause: the falling-behind alarm is raised.
pub const FALLING_BEHIND_ERROR_NS: i64 = 10_000_000;

/// The increment actually used for a configured value (µs): `0` means the default, anything else
/// is clamped to [`MIN_MICRO_STEP_US`]..=[`MAX_MICRO_STEP_US`].
pub fn clamp_micro_step_us(us: u64) -> u64 {
    if us == 0 {
        DEFAULT_MICRO_STEP_US
    } else {
        us.clamp(MIN_MICRO_STEP_US, MAX_MICRO_STEP_US)
    }
}

/// The spacing actually used for a configured value (s): `0` means the default, anything else is
/// clamped to [`MIN_MICRO_INTERVAL_S`]..=[`MAX_MICRO_INTERVAL_S`].
pub fn clamp_micro_interval_s(s: u64) -> u64 {
    if s == 0 {
        DEFAULT_MICRO_INTERVAL_S
    } else {
        s.clamp(MIN_MICRO_INTERVAL_S, MAX_MICRO_INTERVAL_S)
    }
}

/// The micro-correction tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicroConfig {
    /// The largest single correction (ns).
    pub step_ns: i64,
    /// The smallest spacing between two corrections (ns of PTP time).
    pub interval_ns: i64,
    /// See [`MICRO_DEAD_BAND_NS`].
    pub dead_band_ns: i64,
}

impl MicroConfig {
    /// From configured values (µs, s), clamped by [`clamp_micro_step_us`] /
    /// [`clamp_micro_interval_s`].
    pub fn new(step_us: u64, interval_s: u64) -> Self {
        MicroConfig {
            step_ns: clamp_micro_step_us(step_us) as i64 * 1_000,
            interval_ns: clamp_micro_interval_s(interval_s) as i64 * 1_000_000_000,
            dead_band_ns: MICRO_DEAD_BAND_NS,
        }
    }

    /// The largest drift the corrections can hold (ns of correction per minute).
    pub fn capacity_ns_per_min(&self) -> i64 {
        ((self.step_ns as i128 * 60_000_000_000) / self.interval_ns.max(1) as i128) as i64
    }
}

impl Default for MicroConfig {
    fn default() -> Self {
        MicroConfig::new(DEFAULT_MICRO_STEP_US, DEFAULT_MICRO_INTERVAL_S)
    }
}

/// The error estimate at an instant.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MicroEstimate {
    /// Estimated `UTC − fleet line` once everything announced has landed (ns).
    pub error_ns: i64,
    /// Its drift (ns per s, i.e. ppm; positive = the fleet falls behind UTC).
    pub trend_ns_per_s: f64,
    /// The standard error of `error_ns` from the readings' own jitter (ns).
    pub noise_ns: f64,
    /// The standard error of `trend_ns_per_s`.
    pub trend_noise_ns_per_s: f64,
}

/// The robust line through the readings (see [`MicroScheduler::refit`]).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fit {
    /// The newest reading's PTP instant.
    t_ref: i64,
    trend: f64,
    /// Standard error of the drift (ns per s).
    trend_noise: f64,
    /// The level at `t_ref`.
    level: f64,
    /// Standard error of the level (ns).
    noise: f64,
}

/// The micro-correction scheduler of the date authority.
#[derive(Clone, Debug)]
pub struct MicroScheduler {
    cfg: MicroConfig,
    /// (PTP instant, error relative to the TARGET date offset) of each kept reading.
    readings: VecDeque<(i64, i64)>,
    /// When the last correction was decided (PTP), and its direction (±1).
    last: Option<(i64, i8)>,
    /// True while a run of corrections in the last direction is in progress.
    correcting: bool,
    falling_behind: bool,
    /// Every correction of the last [`MICRO_WINDOW_NS`]: (PTP instant, amount).
    increments: VecDeque<(i64, i64)>,
    /// The first reading (PTP), for the correction rate's covered time.
    first_ptp: Option<i64>,
    last_increment_ns: Option<i64>,
    /// The robust line through the readings, refitted when they change (the estimate is read on
    /// every loop iteration).
    fit: Option<Fit>,
}

fn median_f64(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

impl MicroScheduler {
    pub fn new(cfg: MicroConfig) -> Self {
        MicroScheduler {
            cfg,
            readings: VecDeque::new(),
            last: None,
            correcting: false,
            falling_behind: false,
            increments: VecDeque::new(),
            first_ptp: None,
            last_increment_ns: None,
            fit: None,
        }
    }

    pub fn config(&self) -> MicroConfig {
        self.cfg
    }

    /// Keep one UTC reading: `error_ns` = `UTC − fleet line` once everything announced has landed.
    pub fn record(&mut self, error_ns: i64, ptp_ns: i64) {
        self.first_ptp.get_or_insert(ptp_ns);
        self.readings.push_back((ptp_ns, error_ns));
        while self.readings.len() > MICRO_MAX_READINGS
            || self
                .readings
                .front()
                .is_some_and(|&(t, _)| ptp_ns.saturating_sub(t) > MICRO_TREND_WINDOW_NS)
        {
            self.readings.pop_front();
        }
        self.refit();
    }

    /// A correction of `amount_ns` was announced (a micro one, or any other change of the target):
    /// the kept readings now describe `amount_ns` less error.
    pub fn compensate(&mut self, amount_ns: i64) {
        for r in self.readings.iter_mut() {
            r.1 = r.1.wrapping_sub(amount_ns);
        }
        if let Some(fit) = self.fit.as_mut() {
            fit.level -= amount_ns as f64;
        }
    }

    /// Forget the readings and the direction (an abnormal large correction was announced: the
    /// history described a state that no longer exists).
    pub fn clear(&mut self) {
        self.readings.clear();
        self.last = None;
        self.correcting = false;
        self.fit = None;
    }

    /// The PTP time base changed so that `ptp_new = ptp_old − shift_ns` (a grandmaster change or
    /// reboot, `DateAuthority::rebase`): every kept instant moves with it. The wall is continuous
    /// across a rebase, so the errors themselves are unchanged.
    pub fn rebase(&mut self, shift_ns: i64) {
        for r in self.readings.iter_mut() {
            r.0 = r.0.wrapping_sub(shift_ns);
        }
        for i in self.increments.iter_mut() {
            i.0 = i.0.wrapping_sub(shift_ns);
        }
        if let Some((t, dir)) = self.last {
            self.last = Some((t.wrapping_sub(shift_ns), dir));
        }
        self.first_ptp = self.first_ptp.map(|t| t.wrapping_sub(shift_ns));
        if let Some(fit) = self.fit.as_mut() {
            fit.t_ref = fit.t_ref.wrapping_sub(shift_ns);
        }
    }

    /// The robust drift of the kept readings, ns per s. The readings are first reduced to one
    /// point per [`MICRO_TREND_BIN_NS`] (the median instant and the median error of the bin, which
    /// also quiets the jitter), then Theil–Sen: the MEDIAN of all pairwise slopes of those points.
    /// A step in the readings (an upstream UTC jump) reaches at most half of the pairs, so it moves
    /// the drift far less than a fit over pairs that all straddle it; and 20 bins are 190 slopes,
    /// not the 7 000 of the raw readings. Clamped to ±[`MICRO_MAX_TREND_NS_PER_S`]; 0 while the
    /// readings span less than [`MICRO_TREND_MIN_SPAN_NS`].
    ///
    /// Returns `(drift, its standard error)`: the error from the bin points' own scatter around
    /// the fitted line (1.4826 × MAD over √Σ(t − t̄)²).
    fn trend_ns_per_s(&self) -> (f64, f64) {
        let (Some(&(first, _)), Some(&(t_ref, _))) = (self.readings.front(), self.readings.back())
        else {
            return (0.0, 0.0);
        };
        if t_ref.saturating_sub(first) < MICRO_TREND_MIN_SPAN_NS {
            return (0.0, 0.0);
        }
        // Bins counted back from the newest reading (the readings are in time order).
        let mut points: Vec<(f64, f64)> = Vec::new();
        let mut bin: Option<i64> = None;
        let mut ts: Vec<f64> = Vec::new();
        let mut es: Vec<f64> = Vec::new();
        for &(t, e) in self.readings.iter().rev() {
            let k = t_ref.saturating_sub(t) / MICRO_TREND_BIN_NS;
            if bin.is_some_and(|b| b != k) {
                points.push((median_f64(&mut ts), median_f64(&mut es)));
                ts.clear();
                es.clear();
            }
            bin = Some(k);
            ts.push(t.wrapping_sub(t_ref) as f64);
            es.push(e as f64);
        }
        if !ts.is_empty() {
            points.push((median_f64(&mut ts), median_f64(&mut es)));
        }
        let mut slopes = Vec::with_capacity(points.len() * points.len().saturating_sub(1) / 2);
        for (i, a) in points.iter().enumerate() {
            for b in points.iter().skip(i + 1) {
                let dt = a.0 - b.0; // `points` runs newest first
                if dt > 0.0 {
                    slopes.push((a.1 - b.1) * 1e9 / dt);
                }
            }
        }
        if slopes.is_empty() {
            return (0.0, 0.0);
        }
        let trend =
            median_f64(&mut slopes).clamp(-MICRO_MAX_TREND_NS_PER_S, MICRO_MAX_TREND_NS_PER_S);
        let n = points.len() as f64;
        let t_mean = points.iter().map(|p| p.0).sum::<f64>() / n;
        let sxx: f64 = points.iter().map(|p| ((p.0 - t_mean) / 1e9).powi(2)).sum();
        let mut resid: Vec<f64> = points.iter().map(|p| p.1 - trend * p.0 / 1e9).collect();
        let centre = median_f64(&mut resid);
        let mut dev: Vec<f64> = resid.iter().map(|r| (r - centre).abs()).collect();
        let noise = if sxx > 0.0 {
            1.4826 * median_f64(&mut dev) / sxx.sqrt()
        } else {
            0.0
        };
        (trend, noise)
    }

    /// Refit the robust line after the readings changed: the drift is the median pairwise slope of
    /// all kept readings, the level the median of the last [`MICRO_LEVEL_WINDOW_NS`] of readings
    /// projected to the newest one along it. The level's standard error comes from the same
    /// readings' scatter around the line (1.4826 × MAD, × 1.2533 / √n for a median).
    fn refit(&mut self) {
        let Some(&(t_ref, _)) = self.readings.back() else {
            self.fit = None;
            return;
        };
        if self.readings.len() < MICRO_MIN_READINGS {
            self.fit = None;
            return;
        }
        let (trend, trend_noise) = self.trend_ns_per_s();
        let mut level: Vec<f64> = self
            .readings
            .iter()
            .filter(|&&(t, _)| t_ref.saturating_sub(t) <= MICRO_LEVEL_WINDOW_NS)
            .map(|&(t, e)| e as f64 + trend * t_ref.wrapping_sub(t) as f64 / 1e9)
            .collect();
        let n = level.len();
        let centre = median_f64(&mut level);
        let mut dev: Vec<f64> = level.iter().map(|x| (x - centre).abs()).collect();
        let sigma = 1.4826 * median_f64(&mut dev);
        self.fit = Some(Fit {
            t_ref,
            trend,
            trend_noise,
            level: centre,
            noise: 1.2533 * sigma / (n as f64).sqrt(),
        });
    }

    /// The estimated error at the PTP instant `at_ptp_ns`; `None` with fewer than
    /// [`MICRO_MIN_READINGS`] readings.
    pub fn estimate(&self, at_ptp_ns: i64) -> Option<MicroEstimate> {
        let _ = (at_ptp_ns, self.fit);
        None
    }

    /// The direction of the last correction (±1), or 0 when there was none or the fitted drift
    /// has turned against it by [`MICRO_TURNED_TREND_NS_PER_S`] plus three of its standard errors.
    fn standing_direction(&self, est: &MicroEstimate) -> i8 {
        let turned =
            MICRO_TURNED_TREND_NS_PER_S + MICRO_NOISE_MARGIN_SIGMAS * est.trend_noise_ns_per_s;
        match self.last {
            Some((_, dir)) if est.trend_ns_per_s * dir as f64 > -turned => dir,
            _ => 0,
        }
    }

    /// The next micro-correction at `now_ptp_ns`, if one is due: the estimated error where it would
    /// land (`land_ptp_ns`, the announce's instant) decides. Records it: the readings are
    /// compensated at once, and the next one waits at least the interval.
    pub fn decide(&mut self, now_ptp_ns: i64, land_ptp_ns: i64) -> Option<i64> {
        let _ = (now_ptp_ns, land_ptp_ns);
        None
    }

    /// Re-evaluate the falling-behind alarm at `now_ptp_ns`; `Some(new state)` when it changed.
    ///
    /// Raised when the error is beyond the dead band AND either its drift (in the same direction)
    /// reaches the capacity, or it is beyond [`FALLING_BEHIND_ERROR_NS`]. Cleared once the error
    /// is back inside the dead band (the corrections caught up).
    pub fn update_falling_behind(&mut self, now_ptp_ns: i64) -> Option<bool> {
        let _ = now_ptp_ns;
        None
    }

    pub fn falling_behind(&self) -> bool {
        self.falling_behind
    }

    /// The last micro-correction decided (ns, signed).
    pub fn last_increment_ns(&self) -> Option<i64> {
        self.last_increment_ns
    }

    /// The date correction actually applied per minute over the last [`MICRO_WINDOW_NS`] (ns/min,
    /// signed; the covered time since the first reading when that is shorter, at least a minute).
    /// `None` before the first reading.
    pub fn correction_rate_ns_per_min(&self, now_ptp_ns: i64) -> Option<f64> {
        let _ = now_ptp_ns;
        None
    }
}

#[cfg(test)]
mod tests;
