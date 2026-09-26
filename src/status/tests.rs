use super::*;

#[test]
fn test_sync_status_default() {
    let status = SyncStatus::default();
    assert_eq!(status.offset_ns, 0);
    assert_eq!(status.drift_ppm, 0.0);
    assert!(!status.is_locked);
    assert_eq!(status.mode, "ACQ");
}

#[test]
fn test_sync_status_serde_roundtrip() {
    let mut status = SyncStatus::default();
    status.is_locked = true;
    status.mode = "LOCK".to_string();
    status.smoothed_rate_ppm = 2.5;
    status.ntp_offset_us = 150;

    let json = serde_json::to_string(&status).expect("serialize failed");
    let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");

    assert_eq!(restored.is_locked, true);
    assert_eq!(restored.mode, "LOCK");
    assert!((restored.smoothed_rate_ppm - 2.5).abs() < f64::EPSILON);
    assert_eq!(restored.ntp_offset_us, 150);
}

/// dantesync#53: the two new quality fields round-trip like any other
/// field, and an OLD JSON blob missing them entirely (pre-#53) still
/// deserializes cleanly via `#[serde(default)]` — existing consumers
/// (e.g. the tray app's own status struct) must not break.
#[test]
fn test_sync_status_ntp_quality_fields_roundtrip_and_default_on_missing() {
    let status = SyncStatus {
        ntp_offset_us: 100,
        ntp_spread_us: 41610,
        ntp_sample_count: 3,
        ..Default::default()
    };

    let json = serde_json::to_string(&status).expect("serialize failed");
    let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(restored.ntp_spread_us, 41610);
    assert_eq!(restored.ntp_sample_count, 3);

    let old_json = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":false,"updated_ts":0,"is_locked":false,"smoothed_rate_ppm":0.0,
        "ntp_offset_us":0,"mode":"ACQ","ntp_failed":false,"accumulated_phase_us":0.0}"#;
    let restored_old: SyncStatus =
        serde_json::from_str(old_json).expect("old JSON (pre-#53) must still deserialize");
    assert_eq!(restored_old.ntp_spread_us, 0);
    assert_eq!(restored_old.ntp_sample_count, 0);
}

/// dantesync#53 continuation: `pcap_ntp_active` round-trips like any
/// other field, AND an old JSON blob missing it entirely (pre-this-fix)
/// still deserializes cleanly, defaulting to `false` — the honest
/// default (no evidence the pcap transport was ever active).
#[test]
fn test_sync_status_pcap_ntp_active_roundtrips_and_defaults_false_on_missing() {
    let status = SyncStatus {
        pcap_ntp_active: true,
        ..Default::default()
    };

    let json = serde_json::to_string(&status).expect("serialize failed");
    let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert!(restored.pcap_ntp_active);

    let old_json = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":false,"updated_ts":0,"is_locked":false,"smoothed_rate_ppm":0.0,
        "ntp_offset_us":0,"mode":"ACQ","ntp_failed":false,"accumulated_phase_us":0.0}"#;
    let restored_old: SyncStatus = serde_json::from_str(old_json)
        .expect("old JSON (pre-pcap_ntp_active) must still deserialize");
    assert!(
        !restored_old.pcap_ntp_active,
        "missing field must default to false, not silently claim the pcap path is active"
    );
}

/// #68: `/status` must let a consumer tell a LIVE NTP reading from a frozen
/// one. Live on strih, `updated_ts` advanced every second (the PTP loop
/// writes it) beside an `ntp_offset_us` that was 18 hours old — and after a
/// restart, beside one that had never been measured at all. Neither state
/// was distinguishable from a healthy node.
#[test]
fn test_sync_status_exposes_ntp_freshness_separately_from_updated_ts_68() {
    let status = SyncStatus {
        updated_ts: 1_786_439_763,
        ntp_offset_us: -34_718,
        ntp_updated_ts: 1_786_374_529,
        ntp_age_s: Some(65_234),
        ..Default::default()
    };

    let json = serde_json::to_string(&status).expect("serialize failed");
    let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(restored.ntp_updated_ts, 1_786_374_529);
    assert_eq!(restored.ntp_age_s, Some(65_234));
    assert_ne!(
        restored.ntp_updated_ts, restored.updated_ts,
        "NTP freshness must be its OWN field — updated_ts is written by the PTP loop"
    );
}

/// A node that has never taken an NTP measurement must say so explicitly
/// (`null`), never imply "measured just now" with a plausible-looking zero.
#[test]
fn test_sync_status_never_measured_reports_null_age_68() {
    let status = SyncStatus::default();
    assert_eq!(status.ntp_updated_ts, 0);
    assert_eq!(status.ntp_age_s, None);
    let json = serde_json::to_string(&status).expect("serialize failed");
    assert!(
        json.contains("\"ntp_age_s\":null"),
        "never-measured must serialize as an explicit null, got: {}",
        json
    );
}

/// #83: `ntp_deadband_us` reports the currently-active step threshold on a
/// server-mode node (present, Some as soon as server mode is configured --
/// it does NOT require a successful NTP check yet), and stays absent
/// (None) on a client node.
#[test]
fn test_sync_status_ntp_deadband_us_roundtrips_and_defaults_none_83() {
    let server_locked = SyncStatus {
        ntp_offset_us: 15_000,
        ntp_deadband_us: Some(25_000),
        ..Default::default()
    };
    let json = serde_json::to_string(&server_locked).expect("serialize failed");
    let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(restored.ntp_deadband_us, Some(25_000));

    let client_or_unstarted = SyncStatus::default();
    assert_eq!(
        client_or_unstarted.ntp_deadband_us, None,
        "client mode (or before the first server-mode check) must report no deadband"
    );
    let json = serde_json::to_string(&client_or_unstarted).expect("serialize failed");
    assert!(
        json.contains("\"ntp_deadband_us\":null"),
        "must serialize as an explicit null, not be omitted, got: {}",
        json
    );
}

/// Additive only: today's JSON (camera-box's DanteSync gate parses it) must
/// keep deserializing unchanged.
#[test]
fn test_sync_status_pre_68_json_still_deserializes() {
    let old_json = r#"{"offset_ns":156875,"drift_ppm":-6.108,"gm_uuid":null,
        "gm_source_ip":null,"settled":true,"updated_ts":1786439763,"is_locked":true,
        "smoothed_rate_ppm":0.166,"ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,
        "accumulated_phase_us":161.14,"ntp_spread_us":0,"ntp_sample_count":0,
        "pcap_ntp_active":false}"#;
    let restored: SyncStatus =
        serde_json::from_str(old_json).expect("pre-#68 JSON must still deserialize");
    assert_eq!(restored.ntp_updated_ts, 0);
    assert_eq!(restored.ntp_age_s, None);
    assert_eq!(restored.mode, "LOCK");
}

/// dantesync#91: the two new step-storm fields are additive. Pre-#91 JSON
/// (which has ntp_deadband_us but neither step field) must still deserialize,
/// defaulting to "no steps counted / not storming"; and a storming master
/// round-trips both fields.
#[test]
fn test_sync_status_step_storm_fields_are_additive_91() {
    let pre_91 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":2500}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_91).expect("pre-#91 JSON must still deserialize");
    assert_eq!(
        restored.ntp_steps_last_hour, None,
        "absent step count must default to None (never measured), not Some(0)"
    );
    assert!(
        !restored.ntp_step_storm,
        "absent storm flag must default to false"
    );

    let storming = SyncStatus {
        ntp_steps_last_hour: Some(159),
        ntp_step_storm: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&storming).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(back.ntp_steps_last_hour, Some(159));
    assert!(back.ntp_step_storm);
}

/// dantesync#97: the phase-slew telemetry fields are additive. A pre-#97 JSON blob (which has
/// the #91 step-storm fields but none of the phase-slew ones) must still deserialize,
/// defaulting to "feature off / idle"; and an enabled, slewing node round-trips all fields.
#[test]
fn test_sync_status_phase_slew_fields_are_additive_97() {
    let pre_97 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":2500,"ntp_steps_last_hour":3,"ntp_step_storm":false}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_97).expect("pre-#97 JSON must still deserialize");
    assert!(
        !restored.phase_slew_enabled,
        "absent phase_slew_enabled must default to false, never claim the feature is on"
    );
    assert_eq!(restored.f_phase_ppm, 0.0);
    assert_eq!(restored.f_phase_p_ppm, 0.0);
    assert_eq!(restored.f_phase_i_ppm, 0.0);
    assert_eq!(restored.f_ptp_ppm, 0.0);
    assert!(!restored.phase_slew_saturated);

    let slewing = SyncStatus {
        phase_slew_enabled: true,
        f_phase_ppm: 22.5,
        f_phase_p_ppm: 2.5,
        f_phase_i_ppm: 20.0,
        f_ptp_ppm: 33.4,
        phase_slew_saturated: false,
        ..Default::default()
    };
    let json = serde_json::to_string(&slewing).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert!(back.phase_slew_enabled);
    assert!((back.f_phase_ppm - 22.5).abs() < f64::EPSILON);
    assert!((back.f_phase_i_ppm - 20.0).abs() < f64::EPSILON);
    assert!((back.f_ptp_ppm - 33.4).abs() < f64::EPSILON);
}

/// dantesync#101: `ntp_step_threshold_us` is additive. A pre-#101 JSON blob (which has the #97
/// phase-slew fields but not this one) must still deserialize, defaulting to None; and a node
/// carrying a client-mode adaptive threshold round-trips it. This is the field camera-box's
/// step-aware DanteSync gate reads for a Windows client that has no journald (camera-box #1129).
#[test]
fn test_sync_status_ntp_step_threshold_us_is_additive_101() {
    let pre_101 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":null,"ntp_steps_last_hour":null,"ntp_step_storm":false,
        "phase_slew_enabled":false,"f_phase_ppm":0.0,"f_phase_p_ppm":0.0,"f_phase_i_ppm":0.0,
        "f_ptp_ppm":0.0,"phase_slew_saturated":false}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_101).expect("pre-#101 JSON must still deserialize");
    assert_eq!(
        restored.ntp_step_threshold_us, None,
        "absent step-threshold field must default to None (a box not yet serving it), never Some(0)"
    );

    // A client carrying its adaptive threshold round-trips, and serializes as an explicit
    // (non-null) value so the camera-box gate can read it.
    let client = SyncStatus {
        ntp_step_threshold_us: Some(3400),
        ntp_deadband_us: None, // a client reports no server-mode deadband, but DOES have a step threshold
        ..Default::default()
    };
    let json = serde_json::to_string(&client).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(back.ntp_step_threshold_us, Some(3400));
    assert!(
        json.contains("\"ntp_step_threshold_us\":3400"),
        "a client's step threshold must serialize as an explicit numeric value, got: {}",
        json
    );

    // Default (nothing computed yet) serializes as explicit null, never omitted.
    let fresh = SyncStatus::default();
    assert_eq!(fresh.ntp_step_threshold_us, None);
    let json = serde_json::to_string(&fresh).expect("serialize failed");
    assert!(
        json.contains("\"ntp_step_threshold_us\":null"),
        "must serialize as an explicit null, not be omitted, got: {}",
        json
    );
}

/// dantesync#114: the clock-alarm fields are additive. A pre-#114 JSON blob
/// (which has the #101 step-threshold field but neither clock-alarm field)
/// must still deserialize, defaulting to "no alarm / 60 s cadence"; and a
/// node in alarm round-trips the snapshot + cadence.
#[test]
fn test_sync_status_clock_alarm_fields_are_additive_114() {
    let pre_114 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":null,"ntp_steps_last_hour":null,"ntp_step_storm":false,
        "phase_slew_enabled":false,"f_phase_ppm":0.0,"f_phase_p_ppm":0.0,"f_phase_i_ppm":0.0,
        "f_ptp_ppm":0.0,"phase_slew_saturated":false,"ntp_step_threshold_us":null}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_114).expect("pre-#114 JSON must still deserialize");
    assert!(
        !restored.clock_alarm.active,
        "absent clock_alarm must default to inactive, never a spurious alarm"
    );
    assert_eq!(restored.clock_alarm.since, None);
    assert_eq!(restored.clock_alarm.reason, "");
    assert_eq!(
        restored.clock_alarm_interval_s, 60,
        "absent cadence must default to 60 s, not 0"
    );

    // A node in alarm round-trips the snapshot + cadence.
    let alarmed = SyncStatus {
        clock_alarm: ClockAlarmStatus {
            active: true,
            since: Some(1_786_400_000),
            reason: "not PTP-locked to the grandmaster".to_string(),
        },
        clock_alarm_interval_s: 60,
        ..Default::default()
    };
    let json = serde_json::to_string(&alarmed).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert!(back.clock_alarm.active);
    assert_eq!(back.clock_alarm.since, Some(1_786_400_000));
    assert_eq!(back.clock_alarm.reason, "not PTP-locked to the grandmaster");
    // The `/status.clock_alarm` object carries exactly {active, since, reason}.
    assert!(
        json.contains("\"clock_alarm\":{\"active\":true,\"since\":1786400000,\"reason\":"),
        "clock_alarm must serialize as {{active,since,reason}}, got: {json}"
    );
}

/// dantesync#113: the hostname-allowlist resolution fields are additive. A
/// pre-#113 JSON blob (with the #114 clock-alarm fields but neither resolution
/// field) must still deserialize, defaulting both to empty; and a node with a
/// resolved + an unresolved hostname round-trips them.
#[test]
fn test_sync_status_gm_allowlist_resolution_fields_are_additive_113() {
    let pre_113 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":null,"ntp_steps_last_hour":null,"ntp_step_storm":false,
        "phase_slew_enabled":false,"f_phase_ppm":0.0,"f_phase_p_ppm":0.0,"f_phase_i_ppm":0.0,
        "f_ptp_ppm":0.0,"phase_slew_saturated":false,"ntp_step_threshold_us":null,
        "clock_alarm":{"active":false,"since":null,"reason":""},"clock_alarm_interval_s":60}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_113).expect("pre-#113 JSON must still deserialize");
    assert!(restored.gm_allowlist_resolved.is_empty());
    assert!(restored.gm_allowlist_unresolved.is_empty());

    let s = SyncStatus {
        gm_allowlist_resolved: vec!["10.77.9.230".parse().unwrap()],
        gm_allowlist_unresolved: vec!["video-clock.lan".to_string()],
        ..Default::default()
    };
    let json = serde_json::to_string(&s).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(
        back.gm_allowlist_resolved,
        vec!["10.77.9.230".parse::<Ipv4Addr>().unwrap()]
    );
    assert_eq!(
        back.gm_allowlist_unresolved,
        vec!["video-clock.lan".to_string()]
    );
}

/// dantesync#88: the date-offset fields are additive. A pre-#88 blob (today's live shape,
/// with the #113 fields) must still deserialize to "no authority known", and a master's
/// published state round-trips.
#[test]
fn test_sync_status_date_offset_fields_are_additive_88() {
    let pre_88 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1786439763,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "ntp_spread_us":0,"ntp_sample_count":0,"pcap_ntp_active":false,"ntp_updated_ts":0,
        "ntp_age_s":null,"ntp_deadband_us":1000,"ntp_steps_last_hour":0,"ntp_step_storm":false,
        "phase_slew_enabled":true,"f_phase_ppm":-11.4,"gm_allowlist_resolved":[],
        "gm_allowlist_unresolved":[]}"#;
    let restored: SyncStatus =
        serde_json::from_str(pre_88).expect("pre-#88 JSON must still deserialize");
    assert_eq!(restored.date_authority, "");
    assert_eq!(restored.date_offset_ns, None);
    assert_eq!(restored.date_offset_seq, None);
    assert_eq!(restored.date_offset_gm_uuid, None);
    assert_eq!(restored.date_step_pending_ns, None);
    assert_eq!(restored.last_date_step_ts, None);
    assert_eq!(restored.date_steps_late, 0);
    assert_eq!(restored.clock_discipline, "");
    assert_eq!(restored.rate_source, "");
    assert!(!restored.ptp_phase_locked);
    assert_eq!(restored.ptp_phase_error_us, None);

    let master = SyncStatus {
        date_authority: "master".to_string(),
        date_offset_ns: Some(1_790_000_000_123_456_789),
        date_offset_seq: Some(4),
        date_offset_effective_ptp_ns: Some(98_765),
        date_step_pending_ns: Some(-51_000_000),
        date_step_due_in_ms: Some(4_200),
        date_offset_error_ms: Some(-51.0),
        date_step_bound_ms: Some(50.0),
        last_date_step_ns: Some(50_500_000),
        last_date_step_ts: Some(1_790_000_000),
        last_date_step_kind: "coordinated".to_string(),
        date_steps_late: 0,
        ..Default::default()
    };
    let json = serde_json::to_string(&master).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(back.date_authority, "master");
    assert_eq!(back.date_offset_ns, Some(1_790_000_000_123_456_789));
    assert_eq!(back.date_offset_seq, Some(4));
    assert_eq!(back.date_step_pending_ns, Some(-51_000_000));
    assert_eq!(back.last_date_step_kind, "coordinated");
}

/// dantesync#119: the slew fields are additive. A v1.9.0 blob (the #88/#117 fields, no slew)
/// must deserialize to "not slewing", and a slewing node's state round-trips.
#[test]
fn test_sync_status_date_slew_fields_are_additive_119() {
    let v190 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1790000000,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "clock_discipline":"ptp_phase_lock","rate_source":"ptp","ptp_phase_locked":true,
        "date_authority":"follower","date_offset_ns":1790000000123456789,"date_offset_seq":7,
        "date_step_pending_ns":null,"date_steps_late":0}"#;
    let restored: SyncStatus =
        serde_json::from_str(v190).expect("v1.9.0 JSON must still deserialize");
    assert!(!restored.date_slew_active);
    assert_eq!(restored.date_slew_remaining_ms, None);
    assert_eq!(restored.date_slew_ppm, None);
    assert_eq!(restored.date_slew_from_ns, None);
    assert_eq!(restored.date_slew_to_ns, None);
    assert_eq!(restored.date_offset_seq, Some(7));

    let slewing = SyncStatus {
        date_slew_active: true,
        date_slew_remaining_ms: Some(37.5),
        date_slew_ppm: Some(100),
        date_slew_from_ns: Some(1_790_000_000_000_000_000),
        date_slew_to_ns: Some(1_789_999_999_949_000_000),
        ..Default::default()
    };
    let json = serde_json::to_string(&slewing).expect("serialize failed");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert!(back.date_slew_active);
    assert_eq!(back.date_slew_remaining_ms, Some(37.5));
    assert_eq!(back.date_slew_ppm, Some(100));
    assert_eq!(back.date_slew_from_ns, Some(1_790_000_000_000_000_000));
    assert_eq!(back.date_slew_to_ns, Some(1_789_999_999_949_000_000));
}

/// dantesync#119 follow-up: the micro-correction fields are additive. A v1.10.0 blob (with the
/// slew fields) must deserialize to "no micro-correction", and a master's state round-trips.
#[test]
fn test_sync_status_date_micro_fields_are_additive_119() {
    let v1100 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1790000000,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "clock_discipline":"ptp_phase_lock","rate_source":"ptp","ptp_phase_locked":true,
        "date_authority":"master","date_offset_ns":1790000000123456789,"date_offset_seq":7,
        "date_slew_active":false,"date_slew_remaining_ms":null,"date_slew_ppm":null,
        "date_steps_late":0}"#;
    let restored: SyncStatus =
        serde_json::from_str(v1100).expect("v1.10.0 JSON must still deserialize");
    assert!(!restored.date_offset_micro);
    assert!(!restored.date_micro_active);
    assert_eq!(restored.date_micro_last_us, None);
    assert_eq!(restored.date_correction_rate_ms_per_min, None);
    assert!(!restored.date_correction_falling_behind);
    assert!(!restored.date_micro_paused);
    assert_eq!(restored.date_offset_seq, Some(7));

    let master = SyncStatus {
        date_offset_micro: true,
        date_micro_active: true,
        date_micro_last_us: Some(-500),
        date_correction_rate_ms_per_min: Some(1.06),
        date_correction_falling_behind: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&master).expect("serialize failed");
    for key in [
        "\"date_micro_active\":true",
        "\"date_micro_last_us\":-500",
        "\"date_correction_rate_ms_per_min\":1.06",
        "\"date_correction_falling_behind\":true",
    ] {
        assert!(json.contains(key), "{key} in {json}");
    }
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert!(back.date_offset_micro);
    assert!(back.date_micro_active);
    assert_eq!(back.date_micro_last_us, Some(-500));
    assert_eq!(back.date_correction_rate_ms_per_min, Some(1.06));
    assert!(back.date_correction_falling_behind);
}

/// dantesync#119 (1.11.1): the step's phase jump is additive — a 1.11.0 blob reads `null`, and
/// a value round-trips.
#[test]
fn test_sync_status_date_step_phase_jump_is_additive_119() {
    let v1110 = r#"{"offset_ns":0,"drift_ppm":0.0,"gm_uuid":null,"gm_source_ip":null,
        "settled":true,"updated_ts":1790000000,"is_locked":true,"smoothed_rate_ppm":0.1,
        "ntp_offset_us":0,"mode":"LOCK","ntp_failed":false,"accumulated_phase_us":0.0,
        "date_micro_active":true,"date_micro_last_us":500,"date_micro_paused":false}"#;
    let restored: SyncStatus =
        serde_json::from_str(v1110).expect("v1.11.0 JSON must still deserialize");
    assert_eq!(restored.date_step_phase_jump_us, None);

    let st = SyncStatus {
        date_step_phase_jump_us: Some(-3.5),
        ..Default::default()
    };
    let json = serde_json::to_string(&st).expect("serialize failed");
    assert!(json.contains("\"date_step_phase_jump_us\":-3.5"), "{json}");
    let back: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");
    assert_eq!(back.date_step_phase_jump_us, Some(-3.5));
}

#[test]
fn test_to_json_bytes_matches_serde_json_to_vec() {
    // #47: the HTTP status endpoint and the named pipe must serve byte-identical
    // JSON. `to_json_bytes()` is the ONE shared implementation both call — pin
    // that it really is just serde_json::to_vec, so a future edit can't quietly
    // fork the two transports' serialization.
    let status = SyncStatus {
        mode: "LOCK".to_string(),
        offset_ns: 4242,
        ..Default::default()
    };

    let via_helper = status.to_json_bytes().expect("to_json_bytes failed");
    let via_direct = serde_json::to_vec(&status).expect("serde_json::to_vec failed");

    assert_eq!(via_helper, via_direct);
}
