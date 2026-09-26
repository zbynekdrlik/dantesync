//! dantesync#119 (1.12) — the fleet date corrected by ONE coordinated step per NIGHT.
//!
//! Owner decision (issue 119, comment 5849932587). The 1.11 micro-corrections (a coordinated step
//! of 500 µs every 20 s) break the Dante Virtual Soundcard on the stream box: every
//! `NtSetSystemTime` starves its ASIO input, and OBS then ratchets its audio buffering (85 → 405
//! ms). One step per 10 min starved nothing. The grandmaster (a PCIe card with no clock input)
//! cannot be locked to UTC, so the fleet date has to be stepped. It is now stepped ONCE A NIGHT.
//!
//! - All day nothing is corrected. The fleet date runs at the Dante tick and drifts from UTC at
//!   the grandmaster's rate (+17.6 ppm on the rig: ~1.5 s a day). Every box shares that drift, so
//!   the boxes still agree to the µs.
//! - In a window that opens at `daily_step_utc` (02:00 by default: 04:00 CEST / 03:00 CET, with
//!   no time-zone library and the 1 h DST shift accepted), the authority announces ONE coordinated
//!   step of the whole estimated error, in EITHER direction, two leads ahead. The #119
//!   backward-slew rule protected live audio during the day; at night a backward step is allowed.
//! - The window is the moment it opens. It stays open for up to [`DAILY_WINDOW_NS`] (30 min) only
//!   while no fresh UTC reading is available then. When UTC comes back inside the window the step
//!   is made then; otherwise that night is skipped, loudly, and the next night makes it.
//! - An error beyond the emergency cap (`daily_emergency_ms`, 5 s by default) is stepped at once,
//!   loudly. `crate::date_offset::DateAuthority::on_utc_error` does that: it is the abnormal-step
//!   path with its cap raised from 2 × the step bound to the emergency cap.
//!
//! The window is read on the FLEET-line wall (`PTP time + D`, the time every box shows), which
//! trails UTC by the accumulated error. So it opens at most that late (≤ ~1.5 s) after
//! `daily_step_utc` in UTC.
//!
//! This module is the DECISION only: pure, explicit time inputs, no I/O. The error estimate is
//! the micro scheduler's robust line (`crate::date_offset::MicroScheduler`), fed exactly as in
//! micro mode.

use super::micro::{MicroEstimate, MICRO_DEAD_BAND_NS, MICRO_NOISE_MARGIN_SIGMAS};

const NS_PER_S: i64 = 1_000_000_000;
/// One UTC day (ns).
pub const DAY_NS: i64 = 86_400 * NS_PER_S;

/// The default start of the nightly window (UTC): 04:00 CEST / 03:00 CET.
pub const DEFAULT_DAILY_STEP_UTC: &str = "02:00";
/// [`DEFAULT_DAILY_STEP_UTC`] as seconds into the UTC day.
pub const DEFAULT_DAILY_STEP_TOD_S: i64 = 2 * 3_600;

/// How long the window stays open when no fresh UTC reading is available at its start. The step
/// is made at the first moment inside it that has one.
pub const DAILY_WINDOW_NS: i64 = 30 * 60 * NS_PER_S;

/// Default emergency cap (ms): an error beyond it is stepped at once, not at night. It sits above
/// a day's drift at any rate this fleet has shown (+17.6 ppm is 1.5 s a day, and even 50 ppm is
/// 4.3 s), so it fires only for an abnormal state (a bad boot, a lost UTC reference).
pub const DEFAULT_DAILY_EMERGENCY_MS: u64 = 5_000;
/// The configurable emergency cap is clamped to this range. Below 1 s a normal day's drift would
/// trip it several times a day; above 1 h it never protects anything.
pub const MIN_DAILY_EMERGENCY_MS: u64 = 1_000;
pub const MAX_DAILY_EMERGENCY_MS: u64 = 3_600_000;

/// No nightly step is made for an estimated error within this (plus the estimate's noise margin):
/// it is not worth a clock event. This happens, for example, after a restart right after the
/// night's step.
pub const DAILY_MIN_STEP_NS: i64 = MICRO_DEAD_BAND_NS;

/// The emergency cap actually used for a configured value (ms): `0` means the default, anything
/// else is clamped to [`MIN_DAILY_EMERGENCY_MS`]..=[`MAX_DAILY_EMERGENCY_MS`].
pub fn clamp_daily_emergency_ms(ms: u64) -> u64 {
    if ms == 0 {
        DEFAULT_DAILY_EMERGENCY_MS
    } else {
        ms.clamp(MIN_DAILY_EMERGENCY_MS, MAX_DAILY_EMERGENCY_MS)
    }
}

/// Parse `daily_step_utc` leniently: `"HH:MM"`, `"HH:MM:SS"` or `"HH"`, surrounding spaces
/// allowed, one or two digits per field. Returns the seconds into the UTC day, or `None` for
/// anything else (the caller then uses [`DEFAULT_DAILY_STEP_TOD_S`] and warns).
pub fn parse_daily_step_utc(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut fields = [0i64; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 2 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        fields[i] = p.parse().ok()?;
    }
    let [h, m, sec] = fields;
    (h < 24 && m < 60 && sec < 60).then_some(h * 3_600 + m * 60 + sec)
}

/// `wall_ns` (Unix epoch, ns) as an RFC 3339 UTC second: `"2026-09-27T02:00:00Z"`.
pub fn format_utc_rfc3339(wall_ns: i64) -> String {
    let secs = wall_ns.div_euclid(NS_PER_S);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days (proleptic Gregorian, days since 1970-01-01).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        tod / 3_600,
        tod % 3_600 / 60,
        tod % 60
    )
}

/// The nightly-step tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DailyConfig {
    /// Where the window opens, as ns into the UTC day.
    pub step_tod_ns: i64,
    /// An error beyond this is stepped at once (ns).
    pub emergency_ns: i64,
}

impl DailyConfig {
    /// From configured values: the window's start in seconds into the UTC day (taken mod one
    /// day), the emergency cap in ms (clamped by [`clamp_daily_emergency_ms`]).
    pub fn new(step_tod_s: i64, emergency_ms: u64) -> Self {
        DailyConfig {
            step_tod_ns: step_tod_s.rem_euclid(86_400) * NS_PER_S,
            emergency_ns: clamp_daily_emergency_ms(emergency_ms) as i64 * 1_000_000,
        }
    }
}

impl Default for DailyConfig {
    fn default() -> Self {
        DailyConfig::new(DEFAULT_DAILY_STEP_TOD_S, DEFAULT_DAILY_EMERGENCY_MS)
    }
}

/// The `system.date_offset.correction` value (and `/status.date_correction_mode`) of each mode.
pub const CORRECTION_DAILY: &str = "daily";
pub const CORRECTION_MICRO: &str = "micro";

/// How the fleet date is corrected (`system.date_offset.correction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionMode {
    /// One coordinated step per night (the default since 1.12).
    Daily(DailyConfig),
    /// The 1.11 micro-corrections (≤ 500 µs, at most one per 20 s).
    Micro,
}

impl CorrectionMode {
    /// The config / `/status` value: `"daily"` or `"micro"`.
    pub fn label(&self) -> &'static str {
        match self {
            CorrectionMode::Daily(_) => CORRECTION_DAILY,
            CorrectionMode::Micro => CORRECTION_MICRO,
        }
    }
}

/// What the nightly scheduler decided at one tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DailyDecision {
    /// Nothing to do now.
    Idle,
    /// Inside the window with a fresh estimate: step the whole estimated error.
    Step { amount_ns: i64 },
    /// Inside the window, but the error is within the minimum: nothing to step tonight.
    NoStep { error_ns: i64 },
    /// Inside the window with no fresh UTC reading: waiting up to its end (fleet wall, ns).
    /// Reported once per window.
    Waiting { window_end_wall_ns: i64 },
    /// The window closed while waiting for UTC: tonight's step is skipped; the next window opens
    /// at `next_window_wall_ns`. Reported once.
    Skipped { next_window_wall_ns: i64 },
}

/// The date authority's nightly scheduler. All instants are FLEET-line wall time (Unix ns), which
/// a grandmaster change never moves, so a rebase leaves this state alone.
#[derive(Clone, Debug)]
pub struct DailyScheduler {
    cfg: DailyConfig,
    /// The start of the window already handled (stepped, not needed, skipped, or past at boot).
    done_window: Option<i64>,
    /// The start of the window that is waiting for UTC.
    waiting_window: Option<i64>,
    /// The last nightly step announced: (the fleet-wall instant it lands on, its size), ns.
    last_step: Option<(i64, i64)>,
}

impl DailyScheduler {
    pub fn new(cfg: DailyConfig) -> Self {
        DailyScheduler {
            cfg,
            done_window: None,
            waiting_window: None,
            last_step: None,
        }
    }

    pub fn config(&self) -> DailyConfig {
        self.cfg
    }

    /// The start of the latest window that opened at or before `wall_ns`.
    fn window_start(&self, wall_ns: i64) -> i64 {
        let k = wall_ns
            .saturating_sub(self.cfg.step_tod_ns)
            .div_euclid(DAY_NS);
        k.saturating_mul(DAY_NS)
            .saturating_add(self.cfg.step_tod_ns)
    }

    /// The decision at the fleet wall `wall_ns`. `estimate` is the estimated error where the step
    /// would land, or `None` without a fresh UTC reading.
    ///
    /// A window already past at the first call (a boot in the afternoon) is taken as handled,
    /// silently; a window is reported skipped only when this scheduler waited in it.
    pub fn decide(&mut self, wall_ns: i64, estimate: Option<MicroEstimate>) -> DailyDecision {
        let start = self.window_start(wall_ns);
        if self.done_window == Some(start) {
            return DailyDecision::Idle;
        }
        if wall_ns.saturating_sub(start) >= DAILY_WINDOW_NS {
            self.done_window = Some(start);
            if self.waiting_window.take() == Some(start) {
                return DailyDecision::Skipped {
                    next_window_wall_ns: start.saturating_add(DAY_NS),
                };
            }
            return DailyDecision::Idle;
        }
        let Some(est) = estimate else {
            if self.waiting_window == Some(start) {
                return DailyDecision::Idle;
            }
            self.waiting_window = Some(start);
            return DailyDecision::Waiting {
                window_end_wall_ns: start.saturating_add(DAILY_WINDOW_NS),
            };
        };
        self.done_window = Some(start);
        self.waiting_window = None;
        let band =
            DAILY_MIN_STEP_NS.saturating_add((MICRO_NOISE_MARGIN_SIGMAS * est.noise_ns) as i64);
        if est.error_ns.abs() <= band {
            return DailyDecision::NoStep {
                error_ns: est.error_ns,
            };
        }
        DailyDecision::Step {
            amount_ns: est.error_ns,
        }
    }

    /// A nightly step of `amount_ns` was announced to land at the fleet wall `landing_wall_ns`.
    pub fn record_step(&mut self, landing_wall_ns: i64, amount_ns: i64) {
        self.last_step = Some((landing_wall_ns, amount_ns));
    }

    /// The last nightly step announced: (the fleet-wall instant it lands on, its size), ns.
    pub fn last_step(&self) -> Option<(i64, i64)> {
        self.last_step
    }

    /// The start of the next window that can still step (fleet wall, ns): the one open now, if
    /// it is not handled yet, else the next one.
    pub fn next_window_wall_ns(&self, wall_ns: i64) -> i64 {
        let start = self.window_start(wall_ns);
        let open = wall_ns.saturating_sub(start) < DAILY_WINDOW_NS;
        if open && self.done_window != Some(start) {
            start
        } else {
            start.saturating_add(DAY_NS)
        }
    }
}

#[cfg(test)]
mod tests;
