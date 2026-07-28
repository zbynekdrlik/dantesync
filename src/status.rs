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

    /// True when NTP sync has failed (can't reach server)
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
