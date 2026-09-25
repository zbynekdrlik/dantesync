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
mod tests {
    use super::*;

    #[test]
    fn test_default_config_servo_values() {
        let config = SystemConfig::default();

        // Verify servo defaults
        assert!((config.servo.kp - 0.0005).abs() < f64::EPSILON);
        assert!((config.servo.ki - 0.00005).abs() < f64::EPSILON);
        assert!((config.servo.max_freq_adj_ppm - 500.0).abs() < f64::EPSILON);
        assert!((config.servo.max_integral_ppm - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_config_filter_values() {
        let config = SystemConfig::default();

        // Common values across platforms
        assert_eq!(config.filters.sample_window_size, 4);
        assert!((config.filters.warmup_secs - 3.0).abs() < f64::EPSILON);

        // Platform-specific values
        #[cfg(windows)]
        {
            assert_eq!(config.filters.calibration_samples, 3); // Quick calibration
            assert_eq!(config.filters.min_delta_ns, 0);
        }
        #[cfg(not(windows))]
        {
            assert_eq!(config.filters.calibration_samples, 0);
            assert_eq!(config.filters.min_delta_ns, 1_000_000);
        }
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = SystemConfig::default();

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&config).expect("serialize failed");
        assert!(json.contains("kp"));
        assert!(json.contains("sample_window_size"));

        // Deserialize back
        let restored: SystemConfig = serde_json::from_str(&json).expect("deserialize failed");

        // Verify values match
        assert!((restored.servo.kp - config.servo.kp).abs() < f64::EPSILON);
        assert!((restored.servo.ki - config.servo.ki).abs() < f64::EPSILON);
        assert_eq!(
            restored.filters.sample_window_size,
            config.filters.sample_window_size
        );
        assert_eq!(
            restored.filters.calibration_samples,
            config.filters.calibration_samples
        );
    }

    #[test]
    fn test_config_custom_values() {
        let json = r#"{
            "servo": {
                "kp": 0.001,
                "ki": 0.0001,
                "max_freq_adj_ppm": 1000.0,
                "max_integral_ppm": 200.0
            },
            "filters": {
                "sample_window_size": 8,
                "min_delta_ns": 500000,
                "calibration_samples": 5,
                "warmup_secs": 5.0
            }
        }"#;

        let config: SystemConfig = serde_json::from_str(json).expect("parse failed");

        assert!((config.servo.kp - 0.001).abs() < f64::EPSILON);
        assert_eq!(config.filters.sample_window_size, 8);
        assert_eq!(config.filters.min_delta_ns, 500000);
        assert_eq!(config.filters.calibration_samples, 5);
        assert!((config.filters.warmup_secs - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_servo_config_clone() {
        let config = SystemConfig::default();
        let cloned = config.clone();

        assert!((cloned.servo.kp - config.servo.kp).abs() < f64::EPSILON);
        assert_eq!(
            cloned.filters.sample_window_size,
            config.filters.sample_window_size
        );
    }

    // ========================================================================
    // NTP SERVER CONFIG TESTS
    // ========================================================================

    #[test]
    fn test_ntp_server_config_default() {
        let config = NtpServerConfig::default();

        assert!(!config.enabled, "NTP server should be disabled by default");
        assert_eq!(config.port, 123, "Default port should be 123");
        assert_eq!(config.stratum, 3, "Default stratum should be 3");
        assert_eq!(
            config.max_step_us, 100_000,
            "#68: a server-mode correction is bounded to 100ms by default"
        );
    }

    /// #68: a master's EXISTING config predates `max_step_us`. It must still
    /// parse (serde default), and default to the bounded 100 ms — never to 0
    /// (unbounded), which would let one bad upstream reading move the whole
    /// fleet in a single step.
    #[test]
    fn test_ntp_server_config_without_max_step_us_defaults_to_bounded_68() {
        let json = r#"{"enabled": true, "port": 123, "stratum": 3}"#;
        let config: NtpServerConfig =
            serde_json::from_str(json).expect("a pre-#68 config must still parse");
        assert_eq!(config.max_step_us, 100_000);
    }

    #[test]
    fn test_ntp_server_config_serde_roundtrip() {
        let config = NtpServerConfig {
            enabled: true,
            port: 1123,
            stratum: 2,
            max_step_us: 250_000,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: NtpServerConfig = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.enabled, config.enabled);
        assert_eq!(restored.port, config.port);
        assert_eq!(restored.stratum, config.stratum);
        assert_eq!(restored.max_step_us, config.max_step_us);
    }

    #[test]
    fn test_ntp_server_config_partial_json() {
        // Test that partial JSON with only enabled field works
        let json = r#"{"enabled": true, "port": 123, "stratum": 3}"#;
        let config: NtpServerConfig = serde_json::from_str(json).expect("parse failed");

        assert!(config.enabled);
        assert_eq!(config.port, 123);
        assert_eq!(config.stratum, 3);
    }

    #[test]
    fn test_ntp_server_config_clone() {
        let config = NtpServerConfig {
            enabled: true,
            port: 8123,
            stratum: 4,
            max_step_us: 100_000,
        };
        let cloned = config.clone();

        assert_eq!(cloned.enabled, config.enabled);
        assert_eq!(cloned.port, config.port);
        assert_eq!(cloned.stratum, config.stratum);
        assert_eq!(cloned.max_step_us, config.max_step_us);
    }

    // ========================================================================
    // HTTP STATUS CONFIG TESTS (#47)
    // ========================================================================

    #[test]
    fn test_http_status_config_default_enabled_and_port() {
        let config = HttpStatusConfig::default();

        // Enabled by default (unlike NtpServerConfig) — read-only, LAN-bound,
        // unattended reads are the entire point of the feature.
        assert!(
            config.enabled,
            "HTTP status endpoint should be enabled by default"
        );
        assert_eq!(config.port, 8898, "Default port should be 8898");
    }

    #[test]
    fn test_http_status_config_serde_roundtrip() {
        let config = HttpStatusConfig {
            enabled: false,
            port: 9000,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: HttpStatusConfig = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.enabled, config.enabled);
        assert_eq!(restored.port, config.port);
    }

    #[test]
    fn test_http_status_config_clone() {
        let config = HttpStatusConfig {
            enabled: true,
            port: 8898,
        };
        let cloned = config.clone();

        assert_eq!(cloned.enabled, config.enabled);
        assert_eq!(cloned.port, config.port);
    }

    // ========================================================================
    // #47 REVIEW FINDING — partial config objects must not fail the whole parse
    // ========================================================================
    //
    // A previous version of HttpStatusConfig (and the pre-existing NtpServerConfig)
    // had NO per-field #[serde(default)] — only the outer Config.http_status /
    // Config.ntp_server_mode fields were `#[serde(default)]`, which only fires when
    // the KEY IS ENTIRELY ABSENT. A config.json with `"http_status": {"enabled":
    // false}` (missing "port") failed to deserialize the WHOLE Config, and
    // load_config()'s fallback then silently overwrote the user's real config file
    // with fresh defaults — losing their actual ntp_server address and any other
    // customization, with no error logged. Confirmed via the pre-fix struct: it
    // returned `Err("missing field \"port\"")` for exactly this input.

    #[test]
    fn test_http_status_config_partial_object_missing_port_still_parses() {
        let json = r#"{"enabled": false}"#;
        let config: HttpStatusConfig =
            serde_json::from_str(json).expect("partial http_status object must still parse");
        assert!(!config.enabled);
        assert_eq!(
            config.port, 8898,
            "missing port must fall back to the default"
        );
    }

    #[test]
    fn test_http_status_config_partial_object_missing_enabled_still_parses() {
        let json = r#"{"port": 9999}"#;
        let config: HttpStatusConfig =
            serde_json::from_str(json).expect("partial http_status object must still parse");
        assert!(
            config.enabled,
            "missing enabled must fall back to the default (true)"
        );
        assert_eq!(config.port, 9999);
    }

    #[test]
    fn test_http_status_config_empty_object_uses_all_defaults() {
        let json = r#"{}"#;
        let config: HttpStatusConfig =
            serde_json::from_str(json).expect("empty http_status object must still parse");
        assert!(config.enabled);
        assert_eq!(config.port, 8898);
    }

    #[test]
    fn test_ntp_server_config_partial_object_missing_port_still_parses() {
        // Same class of bug, pre-existing in NtpServerConfig before this fix.
        let json = r#"{"enabled": true, "stratum": 2}"#;
        let config: NtpServerConfig =
            serde_json::from_str(json).expect("partial ntp_server_mode object must still parse");
        assert!(config.enabled);
        assert_eq!(
            config.port, 123,
            "missing port must fall back to the default"
        );
        assert_eq!(config.stratum, 2);
    }

    // ========================================================================
    // GM ALLOWLIST CONFIG TESTS (camera-box issue 1073)
    // ========================================================================

    #[test]
    fn clock_alarm_interval_defaults_to_60_and_a_pre_114_config_parses() {
        // Default is the owner's "every minute".
        assert_eq!(SystemConfig::default().clock_alarm_interval_s, 60);

        // A pre-#114 config that lacks the key must still parse and default to 60.
        let json = r#"{"gm_allowlist": ["video-clock.lan"]}"#;
        let config: SystemConfig =
            serde_json::from_str(json).expect("a pre-#114 system object must still parse");
        assert_eq!(config.clock_alarm_interval_s, 60);

        // An explicit value round-trips.
        let mut c = SystemConfig::default();
        c.clock_alarm_interval_s = 30;
        let restored: SystemConfig =
            serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(restored.clock_alarm_interval_s, 30);
    }

    #[test]
    fn gm_allowlist_defaults_to_empty_and_unrestricted() {
        let config = SystemConfig::default();
        assert!(
            config.gm_allowlist.is_empty(),
            "default must be empty = unrestricted = historical last-writer-wins"
        );
    }

    #[test]
    fn system_config_without_gm_allowlist_field_still_parses_empty() {
        // Every pre-existing config predates this field: a full `system` object
        // with servo/filters/ntp_stale_secs but no gm_allowlist must parse and
        // default the field to empty (unrestricted) — backward compatible.
        let json = r#"{
            "servo": {"kp": 0.0005, "ki": 0.00005, "max_freq_adj_ppm": 500.0, "max_integral_ppm": 100.0},
            "filters": {"sample_window_size": 4, "min_delta_ns": 1000000, "calibration_samples": 0, "warmup_secs": 3.0},
            "ntp_stale_secs": 180
        }"#;
        let config: SystemConfig =
            serde_json::from_str(json).expect("a pre-1073 system object must still parse");
        assert!(config.gm_allowlist.is_empty());
    }

    #[test]
    fn system_config_with_only_gm_allowlist_parses_defaulting_servo_and_filters() {
        // The realistic rollout shape: the stream box's config gains ONLY a
        // `system.gm_allowlist` without re-specifying servo/filters. Before the
        // #47-style per-sub-object defaults this would have failed the whole
        // parse (missing servo/filters) and load_config would have silently
        // overwritten the real config.
        let json = r#"{"gm_allowlist": ["10.77.9.0/24"]}"#;
        let config: SystemConfig = serde_json::from_str(json)
            .expect("a system object with only gm_allowlist must still parse");
        assert_eq!(config.gm_allowlist, vec!["10.77.9.0/24".to_string()]);
        // servo/filters fell back to their defaults.
        assert!((config.servo.kp - 0.0005).abs() < f64::EPSILON);
        assert_eq!(config.filters.sample_window_size, 4);
        assert_eq!(config.ntp_stale_secs, 180);
    }

    #[test]
    fn gm_allowlist_serde_roundtrip_preserves_entries() {
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["10.77.9.184".to_string(), "10.77.10.0/24".to_string()];
        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: SystemConfig = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(restored.gm_allowlist, config.gm_allowlist);
    }

    // ========================================================================
    // DSCP CONFIG TESTS (dantesync#52)
    // ========================================================================

    #[test]
    fn system_config_without_dscp_field_defaults_to_enabled_ef() {
        // Every pre-#52 config predates system.dscp: a system object without it
        // must parse and default marking ON at EF/46 (backward compatible).
        let json = r#"{"gm_allowlist": ["10.77.9.184"]}"#;
        let config: SystemConfig =
            serde_json::from_str(json).expect("a pre-#52 system object must still parse");
        assert!(config.dscp.enabled);
        assert_eq!(config.dscp.dscp, 46);
    }

    #[test]
    fn system_config_with_partial_dscp_object_parses() {
        // A box overriding only the code point (CS7) leaves `enabled` defaulting true.
        let json = r#"{"dscp": {"dscp": 56}}"#;
        let config: SystemConfig =
            serde_json::from_str(json).expect("a partial dscp object must still parse");
        assert!(config.dscp.enabled);
        assert_eq!(config.dscp.dscp, 56);
    }

    #[test]
    fn system_config_can_disable_dscp() {
        let json = r#"{"dscp": {"enabled": false}}"#;
        let config: SystemConfig = serde_json::from_str(json).expect("dscp disable must parse");
        assert!(!config.dscp.enabled);
        assert_eq!(config.dscp.dscp, 46); // value defaulted, marking off
    }

    #[test]
    fn dscp_serde_roundtrip_preserves_values() {
        let mut config = SystemConfig::default();
        config.dscp = DscpConfig {
            enabled: true,
            dscp: 56,
        };
        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: SystemConfig = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(restored.dscp, config.dscp);
    }

    // ========================================================================
    // PHASE SLEW CONFIG TESTS (dantesync#97)
    // ========================================================================

    #[test]
    fn phase_slew_defaults_to_disabled() {
        let config = SystemConfig::default();
        assert!(
            !config.phase_slew.enabled,
            "default MUST be disabled — merging #97 changes nothing on the fleet until a box opts in"
        );
        assert!(!PhaseSlewConfig::default().enabled);
    }

    #[test]
    fn system_config_without_phase_slew_field_still_parses_disabled() {
        // Every pre-#97 config predates this key: a full `system` object with servo/filters/
        // ntp_stale_secs/gm_allowlist but no phase_slew must parse and default it to DISABLED.
        let json = r#"{
            "servo": {"kp": 0.0005, "ki": 0.00005, "max_freq_adj_ppm": 500.0, "max_integral_ppm": 100.0},
            "filters": {"sample_window_size": 4, "min_delta_ns": 1000000, "calibration_samples": 0, "warmup_secs": 3.0},
            "ntp_stale_secs": 180,
            "gm_allowlist": []
        }"#;
        let config: SystemConfig =
            serde_json::from_str(json).expect("a pre-#97 system object must still parse");
        assert!(!config.phase_slew.enabled);
    }

    #[test]
    fn system_config_with_only_phase_slew_parses_defaulting_the_rest() {
        // The realistic canary shape: a box's config gains ONLY `system.phase_slew.enabled=true`.
        let json = r#"{"phase_slew": {"enabled": true}}"#;
        let config: SystemConfig = serde_json::from_str(json)
            .expect("a system object with only phase_slew must still parse");
        assert!(config.phase_slew.enabled);
        // the rest fell back to defaults
        assert!((config.servo.kp - 0.0005).abs() < f64::EPSILON);
        assert_eq!(config.filters.sample_window_size, 4);
        assert!(config.gm_allowlist.is_empty());
    }

    #[test]
    fn phase_slew_empty_object_defaults_to_disabled() {
        // `system.phase_slew: {}` (present but no fields) must still parse to disabled.
        let json = r#"{"phase_slew": {}}"#;
        let config: SystemConfig = serde_json::from_str(json).expect("empty phase_slew must parse");
        assert!(!config.phase_slew.enabled);
    }

    #[test]
    fn phase_slew_serde_roundtrip() {
        let mut config = SystemConfig::default();
        config.phase_slew.enabled = true;
        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: SystemConfig = serde_json::from_str(&json).expect("deserialize failed");
        assert!(restored.phase_slew.enabled);
    }

    // ---- dantesync#117 / #88 ---------------------------------------------------------------

    #[test]
    fn clock_discipline_defaults_to_the_ptp_phase_lock_117() {
        let c = SystemConfig::default();
        assert_eq!(c.clock_discipline, "ptp_phase_lock");
        assert!(!c.legacy_clock_discipline());
        assert_eq!(c.unknown_clock_discipline(), None);
    }

    #[test]
    fn a_pre_117_config_parses_into_the_phase_lock_with_default_date_tuning_117() {
        // Today's live shape: phase_slew enabled, no clock_discipline / date_offset keys.
        let json = r#"{"servo":{"kp":0.0005,"ki":0.00005,"max_freq_adj_ppm":500.0,"max_integral_ppm":100.0},
            "filters":{"sample_window_size":4,"min_delta_ns":1000000,"calibration_samples":0,"warmup_secs":3.0},
            "ntp_stale_secs":180,"gm_allowlist":["10.77.9.0/24"],"phase_slew":{"enabled":true}}"#;
        let c: SystemConfig = serde_json::from_str(json).expect("pre-#117 config must parse");
        assert!(!c.legacy_clock_discipline());
        assert_eq!(c.date_offset.step_bound_ns(), 50_000_000);
        assert_eq!(c.date_offset.step_lead_ns(), 5_000_000_000);
        assert!(c.phase_slew.enabled, "kept, but ignored by the phase lock");
    }

    #[test]
    fn legacy_is_an_explicit_opt_in_and_a_typo_means_the_default_117() {
        let legacy: SystemConfig =
            serde_json::from_str(r#"{"clock_discipline":" Legacy "}"#).expect("parses");
        assert!(legacy.legacy_clock_discipline());
        assert_eq!(legacy.unknown_clock_discipline(), None);

        let typo: SystemConfig = serde_json::from_str(r#"{"clock_discipline":"legcy"}"#)
            .expect("a typo must not fail the parse");
        assert!(!typo.legacy_clock_discipline());
        assert_eq!(typo.unknown_clock_discipline(), Some("legcy"));
    }

    #[test]
    fn a_wrongly_typed_new_key_never_fails_the_whole_config_117_88() {
        for bad in [
            r#"{"clock_discipline": true}"#,
            r#"{"clock_discipline": 7}"#,
            r#"{"clock_discipline": ["legacy"]}"#,
            r#"{"clock_discipline": null}"#,
        ] {
            let c: SystemConfig = serde_json::from_str(bad).expect("must still parse");
            assert!(
                !c.legacy_clock_discipline(),
                "{bad}: the default discipline"
            );
            assert!(
                c.unknown_clock_discipline().is_some(),
                "{bad}: reported as unknown"
            );
        }
        let c: SystemConfig =
            serde_json::from_str(r#"{"date_offset":{"step_bound_ms":"fifty","step_lead_ms":-3}}"#)
                .expect("must still parse");
        assert_eq!(c.date_offset.step_bound_ns(), 50_000_000);
        assert_eq!(c.date_offset.step_lead_ns(), 5_000_000_000);
        for bad in [
            r#"{"date_offset": null}"#,
            r#"{"date_offset": "x"}"#,
            r#"{"date_offset": []}"#,
            r#"{"date_offset": 5}"#,
        ] {
            let c: SystemConfig = serde_json::from_str(bad).expect("must still parse");
            assert_eq!(c.date_offset.step_bound_ns(), 50_000_000, "{bad}");
        }
        let c: SystemConfig =
            serde_json::from_str(r#"{"date_offset":{"step_bound_ms":20.0,"step_lead_ms":9000.4}}"#)
                .expect("parses");
        assert_eq!(
            c.date_offset.step_bound_ns(),
            20_000_000,
            "a float is accepted"
        );
        assert_eq!(c.date_offset.step_lead_ns(), 9_000_000_000);
    }

    #[test]
    fn date_offset_tuning_is_floored_and_zero_means_default_88() {
        let c: SystemConfig =
            serde_json::from_str(r#"{"date_offset":{"step_bound_ms":0,"step_lead_ms":100}}"#)
                .expect("parses");
        assert_eq!(c.date_offset.step_bound_ns(), 50_000_000);
        assert_eq!(
            c.date_offset.step_lead_ns(),
            5_000_000_000,
            "lead floored at 5 s"
        );
        let c: SystemConfig =
            serde_json::from_str(r#"{"date_offset":{"step_bound_ms":20}}"#).expect("parses");
        assert_eq!(c.date_offset.step_bound_ns(), 20_000_000);
        assert_eq!(c.date_offset.step_lead_ns(), 5_000_000_000);
    }
}
