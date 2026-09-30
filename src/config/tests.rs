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
    let restored: SystemConfig = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
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
    let config: SystemConfig =
        serde_json::from_str(json).expect("a system object with only phase_slew must still parse");
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

#[test]
fn date_slew_ppm_defaults_to_100_and_is_clamped_to_10_500_119() {
    // A v1.9.0 config has no slew key: the default.
    let c: SystemConfig =
        serde_json::from_str(r#"{"date_offset":{"step_bound_ms":50}}"#).expect("parses");
    assert_eq!(c.date_offset.slew_ppm(), 100);
    assert_eq!(SystemConfig::default().date_offset.slew_ppm(), 100);
    for (raw, want) in [
        ("0", 100),
        ("5", 10),
        ("10", 10),
        ("250", 250),
        ("250.4", 250),
        ("500", 500),
        ("501", 500),
        ("99999999999", 500),
        ("\"fast\"", 100),
        ("-3", 100),
        ("null", 100),
    ] {
        let json = format!(r#"{{"date_offset":{{"slew_ppm":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        assert_eq!(c.date_offset.slew_ppm(), want, "slew_ppm {raw}");
    }
}

#[test]
fn date_micro_step_and_interval_default_to_500_us_and_20_s_and_are_clamped_119() {
    use crate::date_offset::MicroConfig;
    // A v1.10.0 config has no micro keys: the defaults.
    let c: SystemConfig =
        serde_json::from_str(r#"{"date_offset":{"step_bound_ms":50,"slew_ppm":100}}"#)
            .expect("parses");
    assert_eq!(c.date_offset.micro(), MicroConfig::new(500, 20));
    assert_eq!(c.date_offset.micro().step_ns, 500_000);
    assert_eq!(c.date_offset.micro().interval_ns, 20_000_000_000);
    assert_eq!(
        SystemConfig::default().date_offset.micro(),
        MicroConfig::default()
    );
    for (raw, want) in [
        ("0", 500),
        ("10", 50),
        ("200", 200),
        ("200.4", 200),
        ("1000", 1_000),
        ("5000", 1_000),
        ("\"big\"", 500),
        ("-3", 500),
        ("null", 500),
    ] {
        let json = format!(r#"{{"date_offset":{{"micro_step_us":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        assert_eq!(
            c.date_offset.micro().step_ns,
            want * 1_000,
            "micro_step_us {raw}"
        );
    }
    for (raw, want) in [
        ("0", 20),
        ("1", 10),
        ("45", 45),
        ("600", 600),
        ("86400", 600),
        ("[1]", 20),
        ("-1", 20),
    ] {
        let json = format!(r#"{{"date_offset":{{"micro_interval_s":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        assert_eq!(
            c.date_offset.micro().interval_ns,
            want * 1_000_000_000,
            "micro_interval_s {raw}"
        );
    }
}

#[test]
fn date_step_bound_is_floored_so_the_cap_stays_above_the_micro_dead_band_119() {
    for (raw, want_ms) in [("1", 5), ("4", 5), ("5", 5), ("6", 6), ("50", 50)] {
        let json = format!(r#"{{"date_offset":{{"step_bound_ms":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(
            c.date_offset.step_bound_ns(),
            want_ms * 1_000_000,
            "step_bound_ms {raw}"
        );
    }
    // The abnormal cap (2 × the bound) is then never below 10 ms: 5 × the 2 ms dead band.
    assert!(
        crate::date_offset::slew_cap_ns(super::MIN_DATE_STEP_BOUND_MS as i64 * 1_000_000)
            >= 5 * crate::date_offset::MICRO_DEAD_BAND_NS
    );
}

#[test]
fn the_date_is_corrected_once_a_night_by_default_119() {
    use crate::date_offset::{CorrectionMode, DailyConfig};
    // A 1.11 config has no correction keys: the new default, daily at 02:00 UTC, 5 s emergency.
    let c: SystemConfig = serde_json::from_str(
        r#"{"date_offset":{"step_bound_ms":50,"micro_step_us":500,"micro_interval_s":20}}"#,
    )
    .expect("parses");
    let (mode, warnings) = c.date_offset.correction_mode();
    assert_eq!(mode, CorrectionMode::Daily(DailyConfig::default()));
    assert!(warnings.is_empty(), "{warnings:?}");
    let CorrectionMode::Daily(cfg) = mode else {
        unreachable!()
    };
    assert_eq!(cfg.step_tod_ns, 2 * 3_600 * 1_000_000_000);
    assert_eq!(cfg.emergency_ns, 5_000_000_000);
    assert_eq!(
        SystemConfig::default().date_offset.correction_mode().0,
        mode,
        "SystemConfig::default() is daily too"
    );
}

#[test]
fn micro_stays_selectable_and_a_bad_correction_means_daily_loudly_119() {
    use crate::date_offset::{CorrectionMode, DailyConfig};
    for raw in [r#""micro""#, r#"" MICRO ""#] {
        let json = format!(r#"{{"date_offset":{{"correction":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(
            c.date_offset.correction_mode(),
            (CorrectionMode::Micro, vec![]),
            "{raw}"
        );
    }
    for raw in [r#""Daily""#, r#""daily""#] {
        let json = format!(r#"{{"date_offset":{{"correction":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("parses");
        let (mode, warnings) = c.date_offset.correction_mode();
        assert_eq!(mode, CorrectionMode::Daily(DailyConfig::default()), "{raw}");
        assert!(warnings.is_empty(), "{raw}: {warnings:?}");
    }
    // The removed step-bound mode, a typo, a non-string: daily, with a warning naming the value.
    for (raw, needle) in [
        (r#""bound""#, "removed in 1.11.0"),
        (r#""nightly""#, "\"nightly\""),
        ("true", "not a string"),
        ("null", "not a string"),
    ] {
        let json = format!(r#"{{"date_offset":{{"correction":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        let (mode, warnings) = c.date_offset.correction_mode();
        assert_eq!(mode, CorrectionMode::Daily(DailyConfig::default()), "{raw}");
        assert_eq!(warnings.len(), 1, "{raw}: {warnings:?}");
        assert!(warnings[0].contains(needle), "{raw}: {warnings:?}");
    }
}

#[test]
fn daily_step_utc_and_the_emergency_cap_are_lenient_119() {
    use crate::date_offset::{CorrectionMode, DailyConfig};
    let c: SystemConfig = serde_json::from_str(
        r#"{"date_offset":{"daily_step_utc":" 3:30 ","daily_emergency_ms":8000}}"#,
    )
    .expect("parses");
    assert_eq!(
        c.date_offset.correction_mode(),
        (
            CorrectionMode::Daily(DailyConfig::new(3 * 3_600 + 30 * 60, 8_000)),
            vec![]
        )
    );
    // A bad time of day: 02:00, with a warning; the emergency cap clamped, 0 = the default.
    for (raw_tod, raw_ms, want_ms) in [
        (r#""25:00""#, "0", 5_000),
        (r#""02h00""#, "1", 1_000),
        ("7", "\"x\"", 5_000),
        ("null", "-5", 5_000),
    ] {
        let json = format!(
            r#"{{"date_offset":{{"daily_step_utc":{raw_tod},"daily_emergency_ms":{raw_ms}}}}}"#
        );
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        let (mode, warnings) = c.date_offset.correction_mode();
        assert_eq!(
            mode,
            CorrectionMode::Daily(DailyConfig::new(2 * 3_600, want_ms)),
            "{raw_tod} {raw_ms}"
        );
        assert_eq!(warnings.len(), 1, "{raw_tod}: {warnings:?}");
        assert!(warnings[0].contains("daily_step_utc"), "{warnings:?}");
    }
}

#[test]
fn a_follower_holds_the_fleet_date_900_s_by_default_and_the_hold_is_lenient_126() {
    use std::time::Duration;
    // A 1.14 config has no hold key: the default (a master restart or reboot plus its PTP
    // re-acquisition).
    let c: SystemConfig =
        serde_json::from_str(r#"{"date_offset":{"correction":"daily"}}"#).expect("parses");
    assert_eq!(c.date_offset.authority_hold(), Duration::from_secs(900));
    assert_eq!(
        SystemConfig::default().date_offset.authority_hold(),
        Duration::from_secs(super::DEFAULT_AUTHORITY_HOLD_S)
    );
    // 0 = no hold (the 1.14 fallback at 30 s); a day at most; a bad value means the default.
    for (raw, want_s) in [
        ("0", 0),
        ("60", 60),
        ("3600.4", 3_600),
        ("999999", 86_400),
        (r#""x""#, 900),
        ("-5", 900),
        ("null", 900),
        ("[1]", 900),
    ] {
        let json = format!(r#"{{"date_offset":{{"authority_hold_s":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("must still parse");
        assert_eq!(
            c.date_offset.authority_hold(),
            Duration::from_secs(want_s),
            "authority_hold_s {raw}"
        );
    }
}

#[test]
fn a_saved_date_offset_is_restored_within_the_daily_emergency_cap_126() {
    let c: SystemConfig =
        serde_json::from_str(r#"{"date_offset":{"correction":"micro"}}"#).expect("parses");
    assert_eq!(
        c.date_offset.restore_cap_ns(),
        5_000_000_000,
        "the daily emergency cap, whatever the correction mode"
    );
    for (raw, want_ms) in [("8000", 8_000), ("0", 5_000), ("10", 1_000)] {
        let json = format!(r#"{{"date_offset":{{"daily_emergency_ms":{raw}}}}}"#);
        let c: SystemConfig = serde_json::from_str(&json).expect("parses");
        assert_eq!(
            c.date_offset.restore_cap_ns(),
            want_ms * 1_000_000,
            "daily_emergency_ms {raw}"
        );
    }
}
