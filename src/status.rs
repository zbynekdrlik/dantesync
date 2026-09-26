use crate::clock_alarm::ClockAlarmStatus;
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

    /// dantesync#101: this node's OWN currently-active NTP step threshold (microseconds) — the
    /// size of UTC offset it tolerates before it steps its own clock. On a SERVER-mode node this
    /// equals `server_step_threshold_us(is_locked, ptp_offline)` (the same value `ntp_deadband_us`
    /// already reports); on a CLIENT node it is `calculate_ntp_adaptive_threshold()` — the MAD-based
    /// adaptive threshold, the SAME quantity the journal logs as `[NTP] offset:+Nus (threshold:Mus,
    /// adaptive)`. `ntp_deadband_us` (#83) deliberately reports this ONLY in server mode (`None` on
    /// a client); this field fills that gap so a CLIENT's threshold is machine-readable too. Why it
    /// matters: camera-box's DanteSync E2E gate makes a client's median AND stability (spread)
    /// bounds step-aware from this threshold — a Linux cam reads it from journald, but a Windows
    /// client is HTTP-only, so without this field the gate fell back to a fixed 700us term and a
    /// healthy step-straddle spread false-UNSTABLE'd the run (camera-box #1129). `None` only on a
    /// pre-#101 payload (deserialized via `#[serde(default)]`); a live node always reports `Some(..)`.
    #[serde(default)]
    pub ntp_step_threshold_us: Option<i64>,

    // ========================================================================
    // Phase-slew telemetry (dantesync#97) — all additive, all default to the
    // pre-#97 "feature off" reading so an old JSON blob still deserializes and
    // camera-box's DanteSync gate is unaffected until a box opts in.
    // ========================================================================
    /// dantesync#97: true when the bounded PI phase-slew servo is enabled on this node (the
    /// `system.phase_slew.enabled` flag). `false` (default) = the classic step-only UTC path.
    #[serde(default)]
    pub phase_slew_enabled: bool,

    /// dantesync#97: the total commanded phase slew currently composed into the frequency word
    /// (ppm), `P + I` capped to ±200. `0.0` when the servo is disabled or idle.
    #[serde(default)]
    pub f_phase_ppm: f64,

    /// dantesync#97: the proportional part of `f_phase` (ppm) — the fast responder to the current
    /// phase error `e` (which is published as `ntp_offset_us`).
    #[serde(default)]
    pub f_phase_p_ppm: f64,

    /// dantesync#97: the integral part of `f_phase` (ppm) — on a master it converges toward the
    /// node's constant Dante-vs-UTC rate error (≈23 ppm), so the phase error trims to ~0.
    #[serde(default)]
    pub f_phase_i_ppm: f64,

    /// dantesync#97: the PTP frequency-servo correction (ppm) AFTER feed-forward decoupling — the
    /// same value as `drift_ppm`, surfaced explicitly beside `f_phase_*` so the two composed
    /// frequency terms (`f_total = f_ptp + f_phase`) are both readable. `0.0` on a fresh node.
    #[serde(default)]
    pub f_ptp_ppm: f64,

    /// dantesync#97: true while the phase slew is capped at ±200 ppm — a sustained `true` with a
    /// large `ntp_offset_us` is the "slew saturated" condition the `[PHASE-SLEW][SATURATED]` alarm
    /// keys on (the servo cannot keep up; the step path should probably have taken the correction).
    /// dantesync#103: this reflects the servo DEMAND (`P + I` at the cap), NOT the rate-limited slew
    /// actually applied — during the output ramp `f_phase_ppm` can still be small while this is
    /// `true`. That is deliberate (the alarm must arm on the demand, not wait out the ramp); read it
    /// as "the servo is asking for max slew", not "±200 ppm is on the clock right now".
    #[serde(default)]
    pub phase_slew_saturated: bool,

    // ========================================================================
    // Clock alarm (dantesync#114) — all additive, all default to the pre-#114
    // "no alarm / 60 s cadence" reading so an old JSON blob still deserializes
    // and camera-box's DanteSync gate is unaffected.
    // ========================================================================
    /// dantesync#114: the loud "NO DANTE CLOCK" alarm snapshot — `{active, since,
    /// reason}`. `active` is true while this node is NOT PTP-locked to an allowed
    /// grandmaster; `since` is the unix epoch second the current episode began
    /// (`null` when inactive); `reason` is a human string (empty when inactive).
    /// External gates/watchdogs read this to detect a silent fall to NTP-only.
    #[serde(default)]
    pub clock_alarm: ClockAlarmStatus,

    /// dantesync#114: the EFFECTIVE (floored) cadence in seconds at which the
    /// alarm re-notifies while active — so every surface (the daemon WARN log,
    /// the Linux notify-send, the Windows tray balloon) shares one configured
    /// cadence. Default 60.
    #[serde(default = "default_clock_alarm_interval_s")]
    pub clock_alarm_interval_s: u64,

    // ========================================================================
    // Hostname allowlist (dantesync#113) — additive; both default to empty so an
    // old JSON blob still deserializes.
    // ========================================================================
    /// dantesync#113: the CURRENTLY-resolved IPv4 set of every `gm_allowlist`
    /// hostname entry, so an external gate can compare `gm_source_ip` against the
    /// live resolution (empty on a literal-only or not-yet-resolved allowlist).
    #[serde(default)]
    pub gm_allowlist_resolved: Vec<Ipv4Addr>,

    /// dantesync#113: `gm_allowlist` hostname entries that currently FAIL to
    /// resolve (empty when all resolve or there are no hostnames). A non-empty
    /// list is the loud, machine-readable "grandmaster name unresolvable" signal.
    #[serde(default)]
    pub gm_allowlist_unresolved: Vec<String>,

    // ========================================================================
    // Fleet date offset (dantesync#88) — additive; all default to the "no date
    // authority known" reading so an old JSON blob still deserializes.
    // ========================================================================
    /// dantesync#88: this node's role for the fleet date offset `D` (wall = PTP time + D):
    /// `"master"` = the NTP master, the single authority that announces `D`; `"follower"` =
    /// aligned with the master's announced `D` (steps only at the announced instants);
    /// `"local"` = PTP phase-locked but no authority heard yet (an older master, a grandmaster
    /// mismatch, the master unreachable) — the date then follows this node's own NTP step path;
    /// `""` = the legacy discipline, or not PTP-phase-locked yet.
    #[serde(default)]
    pub date_authority: String,

    /// dantesync#88: `D` in effect on this node (ns), `null` until anchored.
    #[serde(default)]
    pub date_offset_ns: Option<i64>,

    /// dantesync#88: the announce this node publishes (the master) or last aligned with (a
    /// follower): its `seq` and its effective PTP instant. `null` until known.
    #[serde(default)]
    pub date_offset_seq: Option<u32>,
    /// dantesync#88: the grandmaster whose PTP time base `date_offset_ns` belongs to (the anchor's
    /// grandmaster — during a grandmaster change it can differ from `gm_uuid` for one window, and
    /// is `null` then). `null` until anchored.
    #[serde(default)]
    pub date_offset_gm_uuid: Option<[u8; 6]>,
    #[serde(default)]
    pub date_offset_effective_ptp_ns: Option<i64>,

    /// dantesync#88: a coordinated date step scheduled on this node: its size (ns) and the time
    /// left until it applies (ms, negative = overdue). `null` when none is scheduled. On the
    /// MASTER this is "the announced fleet D minus my own D": the pending step, or — while the
    /// master's own wall is off the fleet line (its own PTP outage, a failed step) — the way back
    /// to it, with a negative (overdue) due time until it re-aligns. `date_offset_ns +
    /// date_step_pending_ns` is therefore always the D the 31900 extension publishes.
    #[serde(default)]
    pub date_step_pending_ns: Option<i64>,
    #[serde(default)]
    pub date_step_due_in_ms: Option<i64>,

    /// dantesync#88: the FLEET line's UTC error (ms) as the master feeds its authority — its own
    /// reading plus how far its own wall is off the fleet line (equal to its reading while on the
    /// line) — and the bound (ms) past which it announces a step. `null` on a non-master.
    #[serde(default)]
    pub date_offset_error_ms: Option<f64>,
    #[serde(default)]
    pub date_step_bound_ms: Option<f64>,

    /// dantesync#88: the last date step this node applied — size (ns), wall epoch second, and
    /// kind (`"coordinated"`, `"join"`, `"late"`, `"local"`). `null`/empty until one happened.
    #[serde(default)]
    pub last_date_step_ns: Option<i64>,
    #[serde(default)]
    pub last_date_step_ts: Option<u64>,
    #[serde(default)]
    pub last_date_step_kind: String,

    /// dantesync#88: announces this node heard only after their instant and applied late. In a
    /// healthy fleet this stays 0 — every step lands at the announced instant on every box.
    #[serde(default)]
    pub date_steps_late: u32,
    /// dantesync#119: this node is SLEWING the fleet date now (a backward correction: `D` moves at
    /// `date_slew_ppm`, the wall never steps back).
    #[serde(default)]
    pub date_slew_active: bool,
    /// dantesync#119: what this node's slew still has to move `D` (ms); `null` without a slew
    /// scheduled or running.
    #[serde(default)]
    pub date_slew_remaining_ms: Option<f64>,
    /// dantesync#119: the rate (ppm) of the fleet's current slew announce; `null` when the
    /// published date change is not a slew.
    #[serde(default)]
    pub date_slew_ppm: Option<u32>,
    /// dantesync#119: the published slew's start and end `D` (its start instant is
    /// `date_offset_effective_ptp_ns`) — what the 31900 extension carries.
    #[serde(default)]
    pub date_slew_from_ns: Option<i64>,
    #[serde(default)]
    pub date_slew_to_ns: Option<i64>,
    /// dantesync#119 follow-up: the published date change (`date_offset_seq`) is a
    /// MICRO-correction — what the 31900 extension's MICRO flag carries.
    #[serde(default)]
    pub date_offset_micro: bool,
    /// dantesync#119 follow-up: a date micro-correction is in flight on this node now (its step is
    /// scheduled, or its slew scheduled or running).
    #[serde(default)]
    pub date_micro_active: bool,
    /// dantesync#119 follow-up: the last micro-correction this node APPLIED (µs, signed: a forward
    /// step or a backward slew); `null` before the first.
    #[serde(default)]
    pub date_micro_last_us: Option<i64>,
    /// dantesync#119 follow-up (the NTP master only): the fleet date correction actually made per
    /// minute over the last 10 minutes (ms/min, signed) — in steady state the grandmaster-vs-UTC
    /// drift. The capacity is `micro_step_us / micro_interval_s` (1.5 ms/min by default).
    #[serde(default)]
    pub date_correction_rate_ms_per_min: Option<f64>,
    /// dantesync#119 follow-up (the NTP master only): the micro-corrections cannot hold the fleet
    /// date (the drift outruns their capacity, or the error is beyond 10 ms) — journal line
    /// `date correction falling behind`. Never a large step: that exists only beyond 2 × the step
    /// bound.
    #[serde(default)]
    pub date_correction_falling_behind: bool,
    /// dantesync#119 follow-up (the NTP master only): the micro-corrections are paused — no UTC
    /// reading for over a minute (journal line `micro-corrections paused`); the fleet date runs
    /// free at the grandmaster's rate until UTC is back.
    #[serde(default)]
    pub date_micro_paused: bool,

    // ========================================================================
    // PTP phase lock (dantesync#117) — additive.
    // ========================================================================
    /// dantesync#117: `"ptp_phase_lock"` (rate AND phase from the Dante grandmaster, NTP only
    /// moves the date) or `"legacy"` (the pre-#117 rate-only servo + NTP steps / phase_slew).
    /// Empty in a pre-#117 blob.
    #[serde(default)]
    pub clock_discipline: String,

    /// dantesync#117: what steers the frequency word. `"ptp"` whenever NTP has no term in it (the
    /// phase lock, or legacy with phase_slew off); `"ptp+ntp"` for legacy with phase_slew on (the
    /// contract violation #117 removes). Empty in a pre-#117 blob.
    #[serde(default)]
    pub rate_source: String,

    /// dantesync#117: true while the PTP phase lock owns the frequency word (engaged after PTP
    /// lock; the rate servo holds it during acquisition).
    #[serde(default)]
    pub ptp_phase_locked: bool,

    /// dantesync#117: the phase-lock error `e = (t2 − t1) − D` (µs) of the last PTP window;
    /// `null` until anchored. This — not `ntp_offset_us` — is the node's lock quality against the
    /// fleet time line.
    #[serde(default)]
    pub ptp_phase_error_us: Option<f64>,
}

fn default_clock_alarm_interval_s() -> u64 {
    60
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
            // #101: unknown until update_shared_status has computed it
            ntp_step_threshold_us: None,
            // #97: phase slew off / idle by default
            phase_slew_enabled: false,
            f_phase_ppm: 0.0,
            f_phase_p_ppm: 0.0,
            f_phase_i_ppm: 0.0,
            f_ptp_ppm: 0.0,
            phase_slew_saturated: false,
            // #114: no alarm, 60 s cadence by default
            clock_alarm: ClockAlarmStatus::default(),
            clock_alarm_interval_s: default_clock_alarm_interval_s(),
            // #113: no resolved / unresolved hostnames by default
            gm_allowlist_resolved: Vec::new(),
            gm_allowlist_unresolved: Vec::new(),
            // #88: no date authority known until the phase lock anchors
            date_authority: String::new(),
            date_offset_ns: None,
            date_offset_seq: None,
            date_offset_gm_uuid: None,
            date_offset_effective_ptp_ns: None,
            date_step_pending_ns: None,
            date_step_due_in_ms: None,
            date_offset_error_ms: None,
            date_step_bound_ms: None,
            last_date_step_ns: None,
            last_date_step_ts: None,
            last_date_step_kind: String::new(),
            date_steps_late: 0,
            date_slew_active: false,
            date_slew_remaining_ms: None,
            date_slew_ppm: None,
            date_slew_from_ns: None,
            date_slew_to_ns: None,
            date_offset_micro: false,
            date_micro_active: false,
            date_micro_last_us: None,
            date_correction_rate_ms_per_min: None,
            date_correction_falling_behind: false,
            date_micro_paused: false,
            // #117: unknown until the controller publishes
            clock_discipline: String::new(),
            rate_source: String::new(),
            ptp_phase_locked: false,
            ptp_phase_error_us: None,
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
