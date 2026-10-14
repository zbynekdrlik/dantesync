//! The tray's platform-neutral view of the daemon status (dantesync#132).
//!
//! `dantesync-tray` runs on Windows (winit event loop, status over the named pipe
//! `\\.\pipe\dantesync`) and on Linux (gtk main loop, status over the daemon's HTTP
//! endpoint `127.0.0.1:8898/status`). Both shells draw the SAME icon with the SAME
//! colour for the same clock state and show the same menu text, so every decision
//! that does not need a window system lives here, once, with its unit tests:
//!
//! - [`TrayStatus`]: the subset of `/status` the tray reads (serde defaults let an
//!   older daemon's JSON parse);
//! - [`tray_state`] + [`icon_spec`]: the clock state and the icon colour, pulse ring and
//!   update badge for it; no status (daemon unreachable) is always the red offline icon,
//!   never a stale green;
//! - [`render_icon_rgba`]: the 32x32 RGBA pixels of that icon;
//! - [`status_texts`] / [`offline_texts`]: the tooltip and the read-only menu lines;
//! - [`ClockAlarmCadence`]: when the Windows tray raises the dantesync#114 NO DANTE CLOCK
//!   balloon (the Linux daemon already `notify-send`s it itself);
//! - [`fetch_status`]: one HTTP/1.0 GET of the status endpoint with plain `std::net`
//!   (the Linux shell's transport); any failure reads as offline.

use crate::clock_alarm::{compose_message, ClockAlarmStatus};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Width and height of the tray icon in pixels.
pub const ICON_SIZE: u32 = 32;

/// Offline: the daemon does not answer (danger red).
pub const RGB_OFFLINE: [u8; 3] = [220, 53, 69];
/// NTP-only: PTP lost, the date kept by NTP alone (orange).
pub const RGB_NTP_ONLY: [u8; 3] = [255, 152, 0];
/// NANO: ultra-precise PTP lock (cyan).
pub const RGB_NANO: [u8; 3] = [0, 188, 212];
/// LOCK: PTP-locked (green).
pub const RGB_LOCKED: [u8; 3] = [40, 167, 69];
/// Acquiring / settled but not locked (yellow).
pub const RGB_ACQUIRING: [u8; 3] = [255, 193, 7];
/// The update badge dot (orange).
pub const RGB_UPDATE_BADGE: [u8; 3] = [255, 152, 0];

/// The status endpoint the Linux tray polls: the daemon's own HTTP server on loopback
/// (`http_status.rs`, default port 8898).
pub const STATUS_ENDPOINT: &str = "127.0.0.1:8898";

/// The one request the Linux tray sends. HTTP/1.0: the server answers and closes.
pub const STATUS_REQUEST: &[u8] = b"GET /status HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n";

/// The most bytes [`fetch_status`] keeps from one answer (the status JSON is ~2 KiB).
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// The balloon text shown once when the NO DANTE CLOCK alarm clears.
pub const CLOCK_RESTORED_MESSAGE: &str = "Dante clock restored — PTP re-locked";

/// The fastest the clock-alarm balloon repeats, whatever the daemon publishes
/// (the same floor as `clock_alarm::CLOCK_ALARM_INTERVAL_FLOOR_S`).
pub const CLOCK_ALARM_TRAY_FLOOR_S: u64 = 10;

fn default_clock_alarm_interval_s() -> u64 {
    60
}

/// The part of the daemon's `/status` JSON the tray reads. Field names and defaults
/// match `status::SyncStatus`; the fields without a default are required, as they
/// always were for the Windows tray.
#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
pub struct TrayStatus {
    pub offset_ns: i64,
    pub drift_ppm: f64,
    #[serde(rename = "gm_uuid")]
    pub _gm_uuid: Option<[u8; 6]>,
    /// IP address of the PTP grandmaster device.
    #[serde(default)]
    pub gm_source_ip: Option<std::net::Ipv4Addr>,
    pub settled: bool,
    #[serde(rename = "updated_ts")]
    pub _updated_ts: u64,
    #[serde(default)]
    pub is_locked: bool,
    #[serde(default)]
    pub smoothed_rate_ppm: f64,
    #[serde(default)]
    pub ntp_offset_us: i64,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub ntp_failed: bool,
    #[serde(default)]
    pub accumulated_phase_us: f64,
    /// dantesync#114: the NO DANTE CLOCK alarm snapshot.
    #[serde(default)]
    pub clock_alarm: ClockAlarmStatus,
    #[serde(default = "default_clock_alarm_interval_s")]
    pub clock_alarm_interval_s: u64,
}

/// The clock state the icon shows, in decision order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// No status: the daemon is stopped or does not answer.
    Offline,
    /// `mode == "NTP-only"`: PTP is gone.
    NtpOnly,
    /// `mode == "NANO"`.
    Nano,
    /// `is_locked`.
    Locked,
    /// Settled but not locked.
    Settled,
    /// Not settled yet.
    Acquiring,
}

impl TrayState {
    /// A short name for log lines.
    pub fn label(self) -> &'static str {
        match self {
            TrayState::Offline => "OFFLINE",
            TrayState::NtpOnly => "NTP-only",
            TrayState::Nano => "NANO",
            TrayState::Locked => "LOCK",
            TrayState::Settled => "SETTLED",
            TrayState::Acquiring => "ACQUIRING",
        }
    }
}

/// The clock state of one status reading (`None` = offline).
pub fn tray_state(status: Option<&TrayStatus>) -> TrayState {
    match status {
        None => TrayState::Offline,
        Some(s) if s.mode == "NTP-only" => TrayState::NtpOnly,
        Some(s) if s.mode == "NANO" => TrayState::Nano,
        Some(s) if s.is_locked => TrayState::Locked,
        Some(s) if s.settled => TrayState::Settled,
        Some(_) => TrayState::Acquiring,
    }
}

/// What to draw: the fill colour, the pulse ring strength (0 = no ring, 1 = full) and
/// the update badge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconSpec {
    pub rgb: [u8; 3],
    pub pulse: f32,
    pub update_badge: bool,
}

impl IconSpec {
    /// A plain icon: no ring, no badge.
    pub fn plain(rgb: [u8; 3]) -> Self {
        IconSpec {
            rgb,
            pulse: 0.0,
            update_badge: false,
        }
    }

    /// The icon shown before the first status arrives (yellow, connecting).
    pub fn connecting() -> Self {
        IconSpec::plain(RGB_ACQUIRING)
    }

    /// True when two specs draw a visibly identical icon: same colour and badge, and a
    /// pulse ring within 0.05 (about 9 of 255 alpha levels at the ring's inner edge).
    /// The Linux shell skips re-setting an identical icon (each set writes a PNG).
    pub fn same_look(&self, other: &IconSpec) -> bool {
        self.rgb == other.rgb
            && self.update_badge == other.update_badge
            && (self.pulse - other.pulse).abs() < 0.05
    }
}

/// The icon for one status reading (`None` = offline, always red, never a stale state).
///
/// The pulse ring grows with the smoothed drift rate: `|rate| / 20` when locked,
/// `|rate| / 5` in NANO, at least 0.3 while settled but not locked, none otherwise.
pub fn icon_spec(status: Option<&TrayStatus>, update_badge: bool) -> IconSpec {
    let rate = status.map(|s| s.smoothed_rate_ppm.abs()).unwrap_or(0.0);
    let pulse = (rate / 20.0).min(1.0) as f32;
    let (rgb, pulse) = match tray_state(status) {
        TrayState::Offline => (RGB_OFFLINE, 0.0),
        TrayState::NtpOnly => (RGB_NTP_ONLY, 0.0),
        TrayState::Nano => (RGB_NANO, (rate / 5.0).min(1.0) as f32),
        TrayState::Locked => (RGB_LOCKED, pulse),
        TrayState::Settled => (RGB_ACQUIRING, pulse.max(0.3)),
        TrayState::Acquiring => (RGB_ACQUIRING, 0.0),
    };
    IconSpec {
        rgb,
        pulse,
        update_badge,
    }
}

/// The 32x32 RGBA pixels (row-major, 4 bytes each) of an icon: a filled circle, an
/// optional pulsing ring fading outward in a brighter tint of the fill, and an optional
/// orange update badge in the top-right corner. Anti-aliased one pixel at each edge.
pub fn render_icon_rgba(spec: &IconSpec) -> Vec<u8> {
    let [r, g, b] = spec.rgb;
    let size = ICON_SIZE as usize;
    let mut rgba = Vec::with_capacity(size * size * 4);

    let cx = 16.0f32;
    let cy = 16.0f32;
    let inner_radius = 11.0f32;
    let outer_radius = 15.0f32;
    let ring_width = outer_radius - inner_radius;

    let badge_cx = 25.0f32;
    let badge_cy = 7.0f32;
    let badge_radius = 5.0f32;

    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - cx + 0.5;
            let dy = y as f32 - cy + 0.5;
            let dist = (dx * dx + dy * dy).sqrt();

            let badge_dx = x as f32 - badge_cx + 0.5;
            let badge_dy = y as f32 - badge_cy + 0.5;
            let badge_dist = (badge_dx * badge_dx + badge_dy * badge_dy).sqrt();

            if spec.update_badge && badge_dist <= badge_radius {
                let mut alpha = 255u8;
                if badge_dist > badge_radius - 1.0 {
                    alpha = ((badge_radius - badge_dist) * 255.0).max(0.0) as u8;
                }
                let [br, bg, bb] = RGB_UPDATE_BADGE;
                rgba.extend_from_slice(&[br, bg, bb, alpha]);
            } else if dist <= inner_radius {
                let mut alpha = 255u8;
                if dist > inner_radius - 1.0 {
                    alpha = ((inner_radius - dist) * 255.0).max(0.0) as u8;
                }
                rgba.extend_from_slice(&[r, g, b, alpha]);
            } else if dist <= outer_radius && spec.pulse > 0.01 {
                let ring_pos = (dist - inner_radius) / ring_width;
                let ring_alpha = ((1.0 - ring_pos) * spec.pulse * 180.0) as u8;
                let ring_r = (r as f32 * 1.3).min(255.0) as u8;
                let ring_g = (g as f32 * 1.3).min(255.0) as u8;
                let ring_b = (b as f32 * 1.3).min(255.0) as u8;
                rgba.extend_from_slice(&[ring_r, ring_g, ring_b, ring_alpha]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}

/// The tooltip and the three read-only menu lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayTexts {
    pub tooltip: String,
    pub status_line: String,
    pub mode_line: String,
    pub gm_line: String,
}

/// The mode name shown: the daemon's `mode`, or LOCK / ACQ from an older daemon that
/// does not publish one.
pub fn mode_name(status: &TrayStatus) -> &str {
    if status.mode.is_empty() {
        if status.is_locked {
            "LOCK"
        } else {
            "ACQ"
        }
    } else {
        &status.mode
    }
}

/// The texts for a status reading.
pub fn status_texts(status: &TrayStatus, version: &str) -> TrayTexts {
    let mode = mode_name(status);
    let drift = format!("{:+.1}us/s", status.smoothed_rate_ppm);
    let phase = if status.accumulated_phase_us.abs() > 10.0 {
        format!("\nAccum. Phase: {:+.0}us", status.accumulated_phase_us)
    } else {
        String::new()
    };
    let gm = match &status.gm_source_ip {
        Some(ip) => format!("\nPTP Master: {}", ip),
        None => String::new(),
    };
    let tooltip = format!(
        "DanteSync v{}\nMode: {} | Drift: {}\nFreq Adj: {:+.1}ppm\nNTP Offset: {:+}us{}{}",
        version, mode, drift, status.drift_ppm, status.ntp_offset_us, phase, gm
    );
    TrayTexts {
        tooltip,
        status_line: format!("{} | Drift: {}", mode, drift),
        mode_line: format!("Mode: {} | Adj: {:+.1}ppm", mode, status.drift_ppm),
        gm_line: match &status.gm_source_ip {
            Some(ip) => format!("PTP Master: {}", ip),
            None => "PTP Master: --".to_string(),
        },
    }
}

/// The texts while the daemon does not answer.
pub fn offline_texts(version: &str) -> TrayTexts {
    TrayTexts {
        tooltip: format!("DanteSync v{}\nService Offline", version),
        status_line: "Service Offline".to_string(),
        mode_line: "--".to_string(),
        gm_line: "PTP Master: --".to_string(),
    }
}

/// What the clock-alarm cadence asks the tray to show for one status reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmAction {
    /// Nothing to show.
    Quiet,
    /// Raise the NO DANTE CLOCK notification with this text.
    Raise(String),
    /// The alarm just cleared: one reassuring notification ([`CLOCK_RESTORED_MESSAGE`]).
    Cleared,
}

/// The dantesync#114 balloon cadence: while the alarm is active, one notification per
/// interval (floored at [`CLOCK_ALARM_TRAY_FLOOR_S`]) and at once when a new episode
/// starts; one "restored" notification when it clears. Timing uses the monotonic
/// [`Instant`] the caller passes (the daemon steps the wall clock).
#[derive(Debug, Default, Clone)]
pub struct ClockAlarmCadence {
    last_raise: Option<Instant>,
    since: Option<u64>,
}

impl ClockAlarmCadence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide for one status reading.
    pub fn decide(
        &mut self,
        alarm: &ClockAlarmStatus,
        interval_s: u64,
        now: Instant,
    ) -> AlarmAction {
        if alarm.active {
            let interval = Duration::from_secs(interval_s.max(CLOCK_ALARM_TRAY_FLOOR_S));
            let episode_changed = self.since != alarm.since;
            let due = match self.last_raise {
                None => true,
                Some(t) => now.duration_since(t) >= interval,
            };
            if episode_changed || due {
                self.last_raise = Some(now);
                self.since = alarm.since;
                return AlarmAction::Raise(compose_message(&alarm.reason, alarm.since));
            }
            AlarmAction::Quiet
        } else if self.since.is_some() || self.last_raise.is_some() {
            self.last_raise = None;
            self.since = None;
            AlarmAction::Cleared
        } else {
            AlarmAction::Quiet
        }
    }
}

/// Parse one raw HTTP answer of the status endpoint. Anything but a `200` with a status
/// JSON body (another code, no header end, a truncated or foreign body) is `None`.
pub fn parse_status_response(raw: &[u8]) -> Option<TrayStatus> {
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..header_end]).ok()?;
    let mut status_line = head.lines().next()?.split_whitespace();
    let version = status_line.next()?;
    let code = status_line.next()?;
    if !version.starts_with("HTTP/1.") || code != "200" {
        return None;
    }
    serde_json::from_slice(&raw[header_end + 4..]).ok()
}

/// One GET of the status endpoint at `addr`, each step (connect, write, read) bounded by
/// `timeout`. Any failure is `None`: the caller shows offline, never a stale state.
pub fn fetch_status(addr: SocketAddr, timeout: Duration) -> Option<TrayStatus> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    stream.write_all(STATUS_REQUEST).ok()?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.len() > MAX_RESPONSE_BYTES {
                    return None;
                }
            }
            // The server closes right after its answer; a reset or a timeout after the
            // body leaves what arrived, which the JSON parse below accepts only if whole.
            Err(_) => break,
        }
    }
    parse_status_response(&raw)
}

/// Where the Linux tray keeps its single-instance lock: `$XDG_RUNTIME_DIR` (per user)
/// when set and non-empty, else `/tmp`.
pub fn lock_file_path(xdg_runtime_dir: Option<&str>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("dantesync-tray.lock"),
        _ => PathBuf::from("/tmp/dantesync-tray.lock"),
    }
}

#[cfg(test)]
mod tests;
