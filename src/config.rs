use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub servo: ServoConfig,
    pub filters: FilterConfig,
}

/// NTP Server configuration for unified time source mode.
///
/// When enabled, DanteSync becomes an NTP server that:
/// 1. Syncs time ONCE from upstream NTP on startup
/// 2. Stops all periodic NTP queries
/// 3. Serves the PTP-disciplined time to other machines
///
/// Only ONE machine per network should enable this (the "master").
// TODO(#47 review): none of these fields have per-field #[serde(default)] yet — the
// RED state. A config.json with e.g. "ntp_server_mode": {"enabled": true} (missing
// "port"/"stratum") fails the WHOLE Config parse. See the GREEN commit that follows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpServerConfig {
    /// Enable NTP server mode (only one machine per network)
    pub enabled: bool,
    /// Port to listen on (default 123, requires elevated privileges)
    pub port: u16,
    /// Stratum to report to clients (default 3)
    pub stratum: u8,
}

impl Default for NtpServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 123,
            stratum: 3,
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
    pub enabled: bool,
    /// Port to listen on (default 8898 — matches camera-box's existing expectation).
    pub port: u16,
}

impl Default for HttpStatusConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8898,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub sample_window_size: usize,
    pub min_delta_ns: i64,
    pub calibration_samples: usize, // Number of samples for timestamp calibration (0 = disabled)
    pub warmup_secs: f64,           // Warmup period in seconds (0.0 = disabled, for tests)
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

        // Platform-specific values
        #[cfg(windows)]
        let (calibration, min_delta) = (3, 0_i64); // Windows: quick calibration (3 samples ≈ 3s), accept all samples
        #[cfg(not(windows))]
        let (calibration, min_delta) = (0, 1_000_000_i64); // Linux: no calibration, 1ms rate limit

        SystemConfig {
            servo: ServoConfig {
                // Reference values only - controller uses adaptive gains
                kp: 0.0005,
                ki: 0.00005,
                max_freq_adj_ppm: 500.0,
                max_integral_ppm: 100.0,
            },
            filters: FilterConfig {
                // Sample window for median filtering (same on both platforms)
                sample_window_size: 4,

                // Platform-specific rate limiting and calibration
                min_delta_ns: min_delta,
                calibration_samples: calibration,

                // Warmup period (same on both platforms)
                warmup_secs: 3.0,
            },
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
    }

    #[test]
    fn test_ntp_server_config_serde_roundtrip() {
        let config = NtpServerConfig {
            enabled: true,
            port: 1123,
            stratum: 2,
        };

        let json = serde_json::to_string(&config).expect("serialize failed");
        let restored: NtpServerConfig = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.enabled, config.enabled);
        assert_eq!(restored.port, config.port);
        assert_eq!(restored.stratum, config.stratum);
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
        };
        let cloned = config.clone();

        assert_eq!(cloned.enabled, config.enabled);
        assert_eq!(cloned.port, config.port);
        assert_eq!(cloned.stratum, config.stratum);
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
}
