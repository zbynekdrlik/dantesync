use serde::{Deserialize, Serialize};

use crate::dscp::DscpConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    // #47/#68 lesson (see the partial-object tests below): each optional sub-object
    // carries its OWN `#[serde(default)]` so a config that specifies only ONE of
    // them (e.g. a stream box setting just `gm_allowlist`) still deserializes —
    // without this, `{"system": {"gm_allowlist": [...]}}` would fail on the missing
    // `servo`/`filters` and load_config would silently overwrite the real config.
    #[serde(default)]
    pub servo: ServoConfig,
    #[serde(default)]
    pub filters: FilterConfig,
    /// #68 — how long (seconds) a node may go without a successful NTP
    /// measurement before `ntp_failed` is raised and `/status` grades the
    /// reading as stale.
    ///
    /// Default 180 s = 6× the 30 s query cadence, so a couple of missed or slow
    /// bursts never alarm but a genuinely dead NTP path shows within minutes.
    /// Before this, `ntp_failed` had exactly two writers, both inside the query
    /// path — so a node that had simply STOPPED querying (the NTP master, by
    /// design) reported `false` forever while drifting a second off UTC.
    #[serde(default = "default_ntp_stale_secs")]
    pub ntp_stale_secs: u64,

    /// camera-box issue 1073 — trusted grandmaster-source allowlist.
    ///
    /// Each entry is an exact IPv4 (`"10.77.9.184"`) or a CIDR prefix
    /// (`"10.77.9.0/24"`). When non-empty, the PTP client DROPS any Sync/FollowUp
    /// packet whose source IP is not permitted, as-if it never arrived — so a
    /// foreign grandmaster leaking in from another subnet (the live incident: the
    /// stream box seeing mbc's `10.77.7.x` and locking onto `10.77.7.109` instead
    /// of the rig's `10.77.9.184`) can no longer steal the lock.
    ///
    /// EMPTY (the default, and every pre-existing config that lacks the field) =
    /// UNRESTRICTED: accept any source, exactly the historical last-writer-wins
    /// behavior. A single-GM network is unaffected. Parsing is fail-open (an
    /// all-invalid list degrades to unrestricted with a loud warning) so a config
    /// typo can never take the rig's clock offline. See `crate::gm_filter`.
    #[serde(default)]
    pub gm_allowlist: Vec<String>,

    /// dantesync#97 — the phase-slew feature switch (default OFF, per-box canary opt-in).
    ///
    /// Its own `#[serde(default)]` (plus the sub-object's per-field default) means every
    /// pre-#97 config that lacks the key still parses and defaults to DISABLED — with it off the
    /// controller's frequency- and step-paths are byte-for-byte the prior behaviour, so shipping
    /// this changes nothing on the fleet until a box sets `phase_slew.enabled = true`. See
    /// `crate::phase_slew`.
    #[serde(default)]
    pub phase_slew: PhaseSlewConfig,

    /// dantesync#52 — DSCP marking of timesync UDP sockets (default: ON, EF/46).
    ///
    /// Its own `#[serde(default)]` (plus the sub-object's per-field default) means every
    /// pre-#52 config that lacks the key still parses and defaults to marking enabled. See
    /// `crate::dscp` for the coverage split (Linux server-reply effective; Linux rsntp client
    /// and Windows need provisioning-level nftables / QoS policy).
    #[serde(default)]
    pub dscp: DscpConfig,

    /// dantesync#114 — cadence (seconds) of the loud "NO DANTE CLOCK" alarm while
    /// the node is NOT PTP-locked to an allowed grandmaster. Default 60 s (the
    /// owner's "every minute"). Floored at `CLOCK_ALARM_INTERVAL_FLOOR_S` (10 s)
    /// at read time so a `0` can never turn the alarm into per-tick spam.
    ///
    /// This is the ONLY knob — the alarm itself is ALWAYS ON (features-default-on;
    /// no forgettable off switch). Its own `#[serde(default)]` means every pre-#114
    /// config that lacks the key still parses and defaults to 60 s. See
    /// `crate::clock_alarm`.
    #[serde(default = "default_clock_alarm_interval_s")]
    pub clock_alarm_interval_s: u64,

    /// dantesync#117 — how this node disciplines its clock.
    ///
    /// - `"ptp_phase_lock"` (DEFAULT): the owner contract. RATE and PHASE come from the Dante PTP
    ///   grandmaster only (`crate::ptp_phase_lock`); NTP only moves the DATE, through the fleet
    ///   date offset the NTP master announces (`crate::date_offset`, #88). `phase_slew` is never
    ///   engaged in this mode.
    /// - `"legacy"`: the pre-#117 behaviour, kept only for the rollout — the rate-only PTP servo
    ///   plus the NTP step path, with `phase_slew` honoured when enabled.
    ///
    /// A String (not an enum) on purpose: an unknown value must degrade to the default with a
    /// loud warning, never fail the whole config parse (see `config-migration.md`). Read it
    /// through [`SystemConfig::legacy_clock_discipline`].
    #[serde(
        default = "default_clock_discipline",
        deserialize_with = "lenient_string"
    )]
    pub clock_discipline: String,

    /// dantesync#88 — the fleet date-offset authority's tuning (read by the NTP master only).
    /// Lenient as a whole: `null` or a non-object means the defaults, never a failed parse.
    #[serde(default, deserialize_with = "lenient_date_offset")]
    pub date_offset: DateOffsetConfig,
}

/// The `system.clock_discipline` value that selects the new default.
pub const CLOCK_DISCIPLINE_PTP_PHASE_LOCK: &str = "ptp_phase_lock";
/// The `system.clock_discipline` value that restores the pre-#117 behaviour.
pub const CLOCK_DISCIPLINE_LEGACY: &str = "legacy";

fn default_clock_discipline() -> String {
    CLOCK_DISCIPLINE_PTP_PHASE_LOCK.to_string()
}

impl SystemConfig {
    /// True only for an explicit `"legacy"` (case-insensitive, trimmed). Anything else — the
    /// default, or a typo — is the PTP phase lock; `unknown_clock_discipline` reports a typo so
    /// the controller can warn about it.
    pub fn legacy_clock_discipline(&self) -> bool {
        self.clock_discipline
            .trim()
            .eq_ignore_ascii_case(CLOCK_DISCIPLINE_LEGACY)
    }

    /// `Some(value)` when `clock_discipline` is neither known value (it then means the default).
    pub fn unknown_clock_discipline(&self) -> Option<&str> {
        let v = self.clock_discipline.trim();
        if v.eq_ignore_ascii_case(CLOCK_DISCIPLINE_LEGACY)
            || v.eq_ignore_ascii_case(CLOCK_DISCIPLINE_PTP_PHASE_LOCK)
        {
            None
        } else {
            Some(self.clock_discipline.as_str())
        }
    }
}

/// dantesync#88 — the date-offset authority's tuning (ROZHODNUTÉ Q3 defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateOffsetConfig {
    /// The master announces a date step only when |UTC − wall| exceeds this (ms). Default 50.
    /// `0` means the default (a zero bound would announce on every reading).
    #[serde(
        default = "default_date_step_bound_ms",
        deserialize_with = "lenient_bound_ms"
    )]
    pub step_bound_ms: u64,
    /// How far ahead a step is announced (ms). Default 5000; floored at 5000 (every follower
    /// polls once per second and needs several chances to hear it).
    #[serde(
        default = "default_date_step_lead_ms",
        deserialize_with = "lenient_lead_ms"
    )]
    pub step_lead_ms: u64,
    /// dantesync#119 — the rate (ppm) at which a BACKWARD correction is slewed (a backward date
    /// step would lose Dante audio downstream). Default 100 (50 ms in 500 s). `0` means the
    /// default; anything else is clamped to 10..=500 at read time ([`Self::slew_ppm`]).
    #[serde(
        default = "default_date_slew_ppm",
        deserialize_with = "lenient_slew_ppm"
    )]
    pub slew_ppm: u64,
    /// dantesync#119 follow-up — the largest single date MICRO-correction (µs). Default 500. `0`
    /// means the default; anything else is clamped to 50..=1000 at read time ([`Self::micro`]) —
    /// above 1 ms SongPlayer re-anchors, the disturbance the micro-corrections avoid.
    #[serde(
        default = "default_date_micro_step_us",
        deserialize_with = "lenient_micro_step_us"
    )]
    pub micro_step_us: u64,
    /// dantesync#119 follow-up — the smallest spacing between two micro-corrections (s). Default
    /// 20 (with the default step: 1.5 ms/min of capacity). `0` means the default; anything else is
    /// clamped to 10..=600 at read time.
    #[serde(
        default = "default_date_micro_interval_s",
        deserialize_with = "lenient_micro_interval_s"
    )]
    pub micro_interval_s: u64,
}

/// dantesync#117 — a hand-edit like `"clock_discipline": true` must not fail the WHOLE config
/// parse (`load_config` would then fall back to defaults). Any non-string becomes a marker string
/// that `unknown_clock_discipline` reports, so the controller warns and uses the default.
fn lenient_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => s,
        other => format!("<not a string: {}>", other),
    })
}

/// dantesync#88 — the same leniency for the numeric tuning: a non-negative number (an integer, or
/// a float like `10000.0` rounded) is taken; anything else means the default.
fn lenient_u64_or(value: serde_json::Value, default: u64) -> u64 {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f.round() as u64)
        })
        .unwrap_or(default)
}

/// dantesync#88 — `"date_offset": null` / a string / an array must not fail the whole config
/// parse (which would make `load_config` overwrite the file with defaults, losing `ntp_server`).
fn lenient_date_offset<'de, D>(deserializer: D) -> Result<DateOffsetConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

fn lenient_bound_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64_or(
        serde_json::Value::deserialize(deserializer)?,
        default_date_step_bound_ms(),
    ))
}

fn lenient_lead_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64_or(
        serde_json::Value::deserialize(deserializer)?,
        default_date_step_lead_ms(),
    ))
}

fn lenient_slew_ppm<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64_or(
        serde_json::Value::deserialize(deserializer)?,
        default_date_slew_ppm(),
    ))
}

fn lenient_micro_step_us<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64_or(
        serde_json::Value::deserialize(deserializer)?,
        default_date_micro_step_us(),
    ))
}

fn lenient_micro_interval_s<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(lenient_u64_or(
        serde_json::Value::deserialize(deserializer)?,
        default_date_micro_interval_s(),
    ))
}

fn default_date_micro_step_us() -> u64 {
    crate::date_offset::DEFAULT_MICRO_STEP_US
}

fn default_date_micro_interval_s() -> u64 {
    crate::date_offset::DEFAULT_MICRO_INTERVAL_S
}

fn default_date_slew_ppm() -> u64 {
    crate::date_offset::DEFAULT_SLEW_PPM as u64
}

fn default_date_step_bound_ms() -> u64 {
    50
}

fn default_date_step_lead_ms() -> u64 {
    5_000
}

impl Default for DateOffsetConfig {
    fn default() -> Self {
        DateOffsetConfig {
            step_bound_ms: default_date_step_bound_ms(),
            step_lead_ms: default_date_step_lead_ms(),
            slew_ppm: default_date_slew_ppm(),
            micro_step_us: default_date_micro_step_us(),
            micro_interval_s: default_date_micro_interval_s(),
        }
    }
}

impl DateOffsetConfig {
    /// The effective bound in ns (`0` → the default).
    pub fn step_bound_ns(&self) -> i64 {
        let ms = if self.step_bound_ms == 0 {
            default_date_step_bound_ms()
        } else {
            self.step_bound_ms
        };
        (ms.min(3_600_000) as i64) * 1_000_000
    }

    /// The effective lead in ns (floored at 5 s).
    pub fn step_lead_ns(&self) -> i64 {
        (self.step_lead_ms.clamp(5_000, 3_600_000) as i64) * 1_000_000
    }

    /// dantesync#119 — the effective slew rate (ppm): `0` → the default (100), else clamped to
    /// `crate::date_offset::MIN_SLEW_PPM..=MAX_SLEW_PPM` (10..=500).
    pub fn slew_ppm(&self) -> u32 {
        crate::date_offset::clamp_slew_ppm(self.slew_ppm.min(u32::MAX as u64) as u32)
    }

    /// dantesync#119 follow-up — the effective micro-correction tuning (`0` → the defaults, else
    /// clamped: step 50..=1000 µs, interval 10..=600 s).
    pub fn micro(&self) -> crate::date_offset::MicroConfig {
        let _ = (self.micro_step_us, self.micro_interval_s);
        crate::date_offset::MicroConfig::default()
    }
}

fn default_ntp_stale_secs() -> u64 {
    180
}

fn default_clock_alarm_interval_s() -> u64 {
    60
}

/// dantesync#97 — phase-slew servo switch. Only the on/off flag is configurable; the servo's
/// gains / caps / rate limits are hard constants in `crate::phase_slew` (the same way the PTP
/// servo's own gains are hardcoded and auto-tuned — see `ServoConfig`'s "legacy" note).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseSlewConfig {
    /// Enable the bounded PI phase-slew servo (default: false). When false the NTP UTC path steps
    /// exactly as before; when true, a sub-50ms error slews instead of stepping.
    #[serde(default = "default_phase_slew_enabled")]
    pub enabled: bool,
}

fn default_phase_slew_enabled() -> bool {
    false
}

impl Default for PhaseSlewConfig {
    fn default() -> Self {
        PhaseSlewConfig {
            enabled: default_phase_slew_enabled(),
        }
    }
}

/// NTP Server configuration for unified time source mode.
///
/// When enabled, DanteSync becomes an NTP server that:
/// 1. Syncs time from upstream NTP on startup, and — since #68 — KEEPS
///    re-querying that upstream on the normal cadence, disciplining itself
///    with bounded corrections
/// 2. Serves the PTP-disciplined time to other machines
///
/// Only ONE machine per network should enable this (the "master").
///
/// #68 — this used to read "syncs ONCE on startup, stops all periodic NTP
/// queries". That was the defect, not a footnote: PTP locks the master's
/// FREQUENCY to the Dante grandmaster, whose rate is not UTC's rate, so with
/// the periodic queries off the master's UTC phase error integrated from boot
/// with nothing subtracting from it (6-19 ppm measured on strih ⇒ ~21 ms 19
/// minutes after a restart, 1.04 s over two days) while `ntp_failed` stayed
/// `false`. "This machine IS the time source" is true of the fleet's mutual
/// coherence and false of UTC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpServerConfig {
    /// Enable NTP server mode (only one machine per network)
    #[serde(default = "default_ntp_server_mode_enabled")]
    pub enabled: bool,
    /// Port to listen on (default 123, requires elevated privileges)
    #[serde(default = "default_ntp_server_mode_port")]
    pub port: u16,
    /// Stratum to report to clients (default 3)
    #[serde(default = "default_ntp_server_mode_stratum")]
    pub stratum: u8,
    /// #68 — upper bound (microseconds) on a SINGLE periodic UTC correction
    /// while in server mode. Default 100 000 µs (100 ms).
    ///
    /// This node's step is the whole fleet's step, so a wrong-but-consistent
    /// upstream reading (one that survives the two-agreeing-samples gate) must
    /// not be able to move every box at once. In steady state the bound never
    /// fires — at 6-19 ppm a 30 s interval accrues only ~0.2-0.6 ms. It only
    /// shapes recovery from a genuinely large error: a 1.04 s offset is worked
    /// off over ~10 minutes of ordinary intervals, unattended, no restart.
    /// `0` (or negative) means unbounded. The boot-time sync is never bounded —
    /// a cold start must land on UTC immediately.
    #[serde(default = "default_ntp_server_mode_max_step_us")]
    pub max_step_us: i64,
}

fn default_ntp_server_mode_enabled() -> bool {
    false
}

fn default_ntp_server_mode_port() -> u16 {
    123
}

fn default_ntp_server_mode_stratum() -> u8 {
    3
}

fn default_ntp_server_mode_max_step_us() -> i64 {
    100_000
}

impl Default for NtpServerConfig {
    fn default() -> Self {
        Self {
            enabled: default_ntp_server_mode_enabled(),
            port: default_ntp_server_mode_port(),
            stratum: default_ntp_server_mode_stratum(),
            max_step_us: default_ntp_server_mode_max_step_us(),
        }
    }
}

/// HTTP status endpoint configuration.
///
/// Serves the SAME `SyncStatus` JSON the named pipe already emits (minus the pipe's
/// 4-byte length prefix), over a plain HTTP GET, bound to the LAN interface
/// (0.0.0.0). This lets automation/CI on a DIFFERENT machine read PTP/NTP lock
/// status without a human or an SMB/named-pipe bridge (dantesync#47).
///
/// Unlike `NtpServerConfig` (opt-in — only ONE machine per network should become
/// the NTP master), this is enabled by DEFAULT: it is read-only, LAN-bound, and the
/// whole point of the feature is unattended reads working out of the box on every
/// box in the fleet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpStatusConfig {
    /// Enable the HTTP status endpoint (default: true — safe, read-only, LAN-only).
    #[serde(default = "default_http_status_enabled")]
    pub enabled: bool,
    /// Port to listen on (default 8898 — matches camera-box's existing expectation).
    #[serde(default = "default_http_status_port")]
    pub port: u16,
}

fn default_http_status_enabled() -> bool {
    true
}

fn default_http_status_port() -> u16 {
    8898
}

impl Default for HttpStatusConfig {
    fn default() -> Self {
        Self {
            enabled: default_http_status_enabled(),
            port: default_http_status_port(),
        }
    }
}

/// Servo configuration - LEGACY FIELDS (not used by controller)
///
/// The controller uses hardcoded adaptive gains that auto-tune based on
/// oscillation detection. These fields exist only for config file backwards
/// compatibility and are not read by the sync algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServoConfig {
    /// Legacy: not used (controller uses adaptive P gain)
    pub kp: f64,
    /// Legacy: not used (controller uses adaptive I gain)
    pub ki: f64,
    /// Legacy: not used (controller uses DRIFT_MAX_PPM constant)
    pub max_freq_adj_ppm: f64,
    /// Legacy: not used (no integral term in current servo)
    pub max_integral_ppm: f64,
}

impl Default for ServoConfig {
    fn default() -> Self {
        // Reference values only — the controller uses adaptive gains (see
        // `SystemConfig::default`'s comment). Kept as the single source of truth
        // so a partial `system` object without a `servo` key deserializes.
        ServoConfig {
            kp: 0.0005,
            ki: 0.00005,
            max_freq_adj_ppm: 500.0,
            max_integral_ppm: 100.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub sample_window_size: usize,
    pub min_delta_ns: i64,
    pub calibration_samples: usize, // Number of samples for timestamp calibration (0 = disabled)
    pub warmup_secs: f64,           // Warmup period in seconds (0.0 = disabled, for tests)
}

impl Default for FilterConfig {
    fn default() -> Self {
        // Platform-specific rate limiting and calibration, unchanged from the
        // values `SystemConfig::default` used inline before they moved here.
        #[cfg(windows)]
        let (calibration, min_delta) = (3, 0_i64); // Windows: quick calibration (3 samples ≈ 3s), accept all samples
        #[cfg(not(windows))]
        let (calibration, min_delta) = (0, 1_000_000_i64); // Linux: no calibration, 1ms rate limit

        FilterConfig {
            sample_window_size: 4,
            min_delta_ns: min_delta,
            calibration_samples: calibration,
            warmup_secs: 3.0,
        }
    }
}

impl Default for SystemConfig {
    fn default() -> Self {
        // UNIFIED CONFIGURATION - Same core behavior on Windows and Linux
        //
        // ARCHITECTURE: Dual-source time synchronization
        // 1. PTP (Dante) → frequency synchronization only (adjust_frequency)
        // 2. NTP → UTC phase alignment (step_clock)
        //
        // CRITICAL: Dante PTP provides DEVICE UPTIME, not UTC time!
        // - NTP handles all time stepping via periodic UTC corrections
        // - PTP locks the frequency while NTP keeps absolute time correct
        //
        // The controller uses ADAPTIVE gains, so kp/ki values here are for reference only.
        // Actual gains are auto-tuned based on oscillation detection.

        SystemConfig {
            // Reference/platform defaults are now the single source of truth in
            // ServoConfig::default / FilterConfig::default (so a partial `system`
            // object deserializes) — reuse them here.
            servo: ServoConfig::default(),
            filters: FilterConfig::default(),

            // #68: 6x the 30s NTP query cadence
            ntp_stale_secs: default_ntp_stale_secs(),

            // camera-box issue 1073: empty = unrestricted (accept any GM source),
            // the historical last-writer-wins behavior. Backward compatible.
            gm_allowlist: Vec::new(),

            // dantesync#97: phase slew is OFF by default — merging it changes nothing on the
            // fleet until a box opts in during the canary rollout.
            phase_slew: PhaseSlewConfig::default(),

            // dantesync#52: DSCP marking ON by default (EF/46). Harmless when a
            // switch ignores DSCP; a bad value fails open to unmarked.
            dscp: DscpConfig::default(),

            // dantesync#114: loud NO-DANTE-CLOCK alarm every 60 s while unlocked.
            clock_alarm_interval_s: default_clock_alarm_interval_s(),

            // dantesync#117: the PTP phase lock is the default; "legacy" restores the old path.
            clock_discipline: default_clock_discipline(),

            // dantesync#88: 50 ms step bound, 5 s announce lead.
            date_offset: DateOffsetConfig::default(),
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests;
