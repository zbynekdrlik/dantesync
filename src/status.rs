use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// Sync status shared via IPC between service and tray app
///
/// This struct contains all the information needed for the tray app to:
/// - Display sync state (locked, acquiring, offline)
/// - Animate the icon based on drift rate
/// - Show detailed status in tooltips and menus
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SyncStatus {
    // ========================================================================
    // Core PTP Status (existing fields)
    // ========================================================================
    /// Current phase offset from Dante master (nanoseconds)
    /// Note: Absolute value is meaningless for Dante (device uptime, not UTC)
    pub offset_ns: i64,

    /// Current frequency adjustment being applied (PPM)
    pub drift_ppm: f64,

    /// Grandmaster clock UUID (from PTP Sync messages)
    pub gm_uuid: Option<[u8; 6]>,

    /// IP address of the device sending PTP Sync messages
    /// This is the actual network address of the PTP grandmaster/boundary clock
    pub gm_source_ip: Option<Ipv4Addr>,

    /// True once sync is established (receiving valid packets)
    pub settled: bool,

    /// Unix timestamp of last status update
    pub updated_ts: u64,

    // ========================================================================
    // Extended Status (new fields for tray app)
    // ========================================================================
    /// True when frequency is locked (rate stable < 5us/s)
    /// Used for icon badge color (green = locked)
    pub is_locked: bool,

    /// Smoothed rate of offset change (us/s)
    /// Used for icon animation speed - higher rate = faster pulse
    pub smoothed_rate_ppm: f64,

    /// Last NTP offset measurement (microseconds)
    /// Used for NTP status display in tray menu
    pub ntp_offset_us: i64,

    /// Current operating mode: "ACQ" (acquiring), "PROD" (production), "LOCK" (locked), "NTP-only"
    /// Used for status display and icon state
    pub mode: String,

    /// True when this node's UTC alignment is NOT being maintained.
    ///
    /// dantesync#68 widened this: it used to mean only "a query returned an
    /// error", which meant a node that had simply STOPPED querying (the NTP
    /// master, by design) reported `false` for 18 hours while drifting a second
    /// off UTC. It now covers BOTH causes:
    ///
    /// - repeated query failures (upstream unreachable — the original meaning), and
    /// - no successful measurement within `system.ntp_stale_secs`, whether or
    ///   not anything was even attempted.
    ///
    /// Read `ntp_age_s` alongside it to tell the two apart, and never read
    /// `ntp_offset_us` without checking one of them first.
    pub ntp_failed: bool,

    /// Accumulated phase error since last NTP step (microseconds)
    /// Tracks estimated UTC drift between NTP corrections
    /// Reset to 0 after each NTP step
    pub accumulated_phase_us: f64,

    /// dantesync#53: spread (max - min, microseconds) across the accepted
    /// (lowest-RTT) burst samples that produced `ntp_offset_us`. Large even
    /// when `ntp_offset_us` looks reasonable ⇒ this node's NTP measurement is
    /// NOT trustworthy despite the filtered value — never read `ntp_offset_us`
    /// alone as "this node is fine".
    #[serde(default)]
    pub ntp_spread_us: u64,

    /// dantesync#53: how many burst samples fed the published `ntp_offset_us`
    /// (<= the configured accept count; lower after a partial burst failure —
    /// also a reason to trust the value less).
    #[serde(default)]
    pub ntp_sample_count: usize,

    /// dantesync#53 continuation: whether the kernel-timestamped Npcap NTP
    /// transport was used for EVERY sample of the last burst (Windows only —
    /// always `false` on platforms with no such transport). `false` here
    /// means every sample fell back to userspace `rsntp`, which is exactly
    /// the silent-degradation failure mode confirmed live on the stream box
    /// (the transport failed at construction — dual-homed host, NTP server
    /// unreachable from the PTP capture NIC — and nothing after the startup
    /// log line ever showed it again). Monitoring/gates should treat a
    /// persistent `false` on a Windows node as worth investigating.
    #[serde(default)]
    pub pcap_ntp_active: bool,

    /// dantesync#68: unix epoch second of the last SUCCESSFUL NTP measurement
    /// (`0` = never measured). This is the field `updated_ts` is NOT: that one
    /// is written by the PTP loop on every status refresh, so it kept advancing
    /// beside an `ntp_offset_us` frozen 18 hours earlier, and a consumer had no
    /// way to tell. Read this (or `ntp_age_s`) before trusting `ntp_offset_us`.
    #[serde(default)]
    pub ntp_updated_ts: u64,

    /// dantesync#68: seconds since that measurement, computed at status-write
    /// time; `null` when nothing has EVER been measured — deliberately not `0`,
    /// which would read as "measured just now". This is the number a monitoring
    /// gate should grade before grading `ntp_offset_us` at all: live on strih
    /// the offset field read a perfect `0` because no measurement had ever been
    /// published, not because the node was on time.
    #[serde(default)]
    pub ntp_age_s: Option<u64>,

    /// dantesync#83: the threshold (microseconds) this node would CURRENTLY apply to a step
    /// decision if server mode is configured -- `None` on a client node (server mode only).
    /// `Some(..)` as soon as `configure_ntp_server_mode()` has run, reflecting whichever
    /// threshold applies given the node's CURRENT `is_locked`/`ptp_offline` state -- it does
    /// NOT require a successful NTP check to have happened yet (review finding, #83: an
    /// earlier draft of this doc claimed `None` "before the first server-mode check", which
    /// was never actually true -- `update_shared_status()` computes this purely from
    /// `server_step_threshold_us(is_locked, ptp_offline)`, independent of `ntp_offset_us`'s
    /// own freshness). A residual in `ntp_offset_us` up to roughly this value is EXPECTED,
    /// healthy behavior, not drift or instability: while genuinely PTP-locked, this node
    /// deliberately uses a large deadband (the Dante grandmaster's own real, unfixable rate
    /// error vs UTC is operationally irrelevant to the fleet's internal consistency, so UTC
    /// phase is corrected only every few minutes instead of every ~20-40s) rather than the
    /// tight tracking used while not yet locked. Any consumer grading `ntp_offset_us` for
    /// stability (e.g. camera-box's own E2E DanteSync gate) should read this field FIRST and
    /// grade against it, not against a fixed assumed bound.
    #[serde(default)]
    pub ntp_deadband_us: Option<i64>,

    /// dantesync#91: how many NTP clock STEPS this node applied in the trailing
    /// hour — the honest "steps/h" health metric #67 asked for. `None` on a
    /// client node (or a node that has never served in server mode). A healthy,
    /// genuinely-PTP-locked master tops out near ~84/h (the 2500us deadband at
    /// the worst-ever 66ppm Dante-GM rate error, #83); a sustained value above
    /// `NTP_STEP_STORM_THRESHOLD_PER_HOUR` means this master's PTP FREQUENCY
    /// reference is degraded (a GM outage drops it to the tight 200us threshold,
    /// which then step-storms at every check) and the whole NTP fleet is chasing
    /// the storm. A dev1 watchdog can grade this directly.
    #[serde(default)]
    pub ntp_steps_last_hour: Option<u32>,

    /// dantesync#91: true while this server-mode node's `ntp_steps_last_hour`
    /// exceeds `NTP_STEP_STORM_THRESHOLD_PER_HOUR` — the step-storm alarm surface
    /// (the loud, grep-able `[NTP][STEP-STORM]` log line is the log-side
    /// equivalent). Always false on a client node. The storm itself can only be
    /// cleared by restoring the PTP grandmaster / frequency reference — this flag
    /// exists so the 19h-silent degradation that motivated #91 pages instead.
    #[serde(default)]
    pub ntp_step_storm: bool,
}

impl SyncStatus {
    /// Serialize this status to the SAME JSON bytes used by every consumer of the
    /// status — the named pipe (Windows tray IPC, length-prefixed) and the HTTP
    /// status endpoint (dantesync#47, plain body). One implementation, shared by
    /// both transports, so they can never silently drift apart.
    pub fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
}

impl Default for SyncStatus {
    fn default() -> Self {
        SyncStatus {
            // Core fields
            offset_ns: 0,
            drift_ppm: 0.0,
            gm_uuid: None,
            gm_source_ip: None,
            settled: false,
            updated_ts: 0,

            // Extended fields for tray app
            is_locked: false,
            smoothed_rate_ppm: 0.0,
            ntp_offset_us: 0,
            mode: "ACQ".to_string(),
            ntp_failed: false,
            accumulated_phase_us: 0.0,
            ntp_spread_us: 0,
            ntp_sample_count: 0,
            pcap_ntp_active: false,
            // #68: nothing measured yet — say so, never imply "just now"
            ntp_updated_ts: 0,
            ntp_age_s: None,
            // #83: unknown until a server-mode check has actually run
            ntp_deadband_us: None,
            // #91: no steps counted / not storming until a server-mode step lands
            ntp_steps_last_hour: None,
            ntp_step_storm: false,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
