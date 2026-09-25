//! UDP Time Query Server for network time verification.
//!
//! This module provides a lightweight UDP server that responds to time queries
//! with precise clock data, enabling independent verification that all computers
//! on the network have the same tick counter value.
//!
//! # Protocol
//!
//! **Port:** 31900 (UDP)
//!
//! **Request Packet:** 8 bytes
//! - `[0-3]` Magic: "DSYN" (0x4453594E)
//! - `[4-7]` Request ID (u32, for matching responses)
//!
//! **Response Packet:** 64 bytes
//! - `[0-3]`   Magic: "DSYR" (0x44535952)
//! - `[4-7]`   Request ID (echo back)
//! - `[8-15]`  System time (UTC nanoseconds since Unix epoch, u64)
//! - `[16-23]` Monotonic counter (QPC on Windows, CLOCK_MONOTONIC_RAW on Linux, u64)
//! - `[24-31]` PTP offset from grandmaster (nanoseconds, signed i64)
//! - `[32-35]` Drift rate (PPM × 1000, signed i32)
//! - `[36-39]` Frequency adjustment (PPM × 1000, signed i32)
//! - `[40]`    Mode: 0=INIT, 1=ACQ, 2=PROD, 3=LOCK, 4=NANO, 5=NTP_ONLY
//! - `[41]`    Is locked: 0/1
//! - `[42-47]` Grandmaster UUID (6 bytes)
//! - `[48-55]` Monotonic frequency (ticks per second, u64)
//! - `[56-59]` NTP offset (microseconds, signed i32)
//! - `[60-61]` Accumulated phase drift since last NTP step (microseconds, signed i16)
//! - `[62]`    Flags: bit 0 = ntp_failed, bit 1 = settled
//! - `[63]`    Reserved (zero)
//!
//! # dantesync#88 — the date-offset extension (versioned, opt-in by the REQUEST)
//!
//! A request whose magic is `"DSYX"` (0x44535958) instead of `"DSYN"` asks for the reply's
//! versioned extension: the same 64-byte base, then — when this node has a date-offset state —
//! the [`crate::date_offset`] extension (`[64]` version, `[65]` flags with bit 0 = the fleet's
//! date-offset AUTHORITY, `[68-75]` `date_offset_ns`, `[76-83]` `effective_ptp_ns`, `[84-87]`
//! `seq`). Compatibility, both directions:
//!
//! - an OLD client sends `"DSYN"` and gets the byte-identical 64-byte reply it always got — it
//!   never sees extra bytes (a 64-byte receive buffer on Windows would otherwise fail the whole
//!   datagram with `WSAEMSGSIZE`, not truncate it);
//! - an OLD server ignores `"DSYX"` as an invalid magic (debug-logged, no reply), so a new client
//!   talking to it simply hears no authority and keeps its local date fallback.
//!
//! A new client reads the extension with [`parse_reply`]; [`UdpAuthorityPoller`] polls the NTP
//! master once per second on a background thread so the sync loop never blocks on DNS or I/O.

use crate::date_offset::{decode_extension, encode_extension, DateAnnounce, DateExtension};
use crate::status::SyncStatus;
use anyhow::Result;
use log::{debug, error, info, warn};
use std::net::{ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// UDP port for time query server
pub const TIME_SERVER_PORT: u16 = 31900;

/// Request magic bytes: "DSYN"
const REQUEST_MAGIC: u32 = 0x4453594E;

/// dantesync#88 — request magic "DSYX": the base reply PLUS the versioned date-offset extension.
const REQUEST_MAGIC_EXT: u32 = 0x44535958;

/// Response magic bytes: "DSYR"
const RESPONSE_MAGIC: u32 = 0x44535952;

/// Minimum request packet size
const REQUEST_SIZE: usize = 8;

/// Response packet size
const RESPONSE_SIZE: usize = 64;

/// dantesync#88 — receive buffer for requests. Larger than any request we accept so a stray
/// oversized datagram never fails the whole `recv_from` on Windows (`WSAEMSGSIZE`).
const REQUEST_BUF_SIZE: usize = 64;

/// UDP Time Query Server for network time verification.
///
/// Listens on port 31900 and responds to time queries with precise clock data
/// including system time, monotonic counter, and sync status.
pub struct TimeServer {
    socket: UdpSocket,
}

impl TimeServer {
    /// Create a new TimeServer bound to UDP port 31900.
    ///
    /// The socket is set to non-blocking mode for integration with the main loop.
    pub fn new() -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", TIME_SERVER_PORT);
        let socket = UdpSocket::bind(&bind_addr)?;
        socket.set_nonblocking(true)?;

        info!(
            "[TimeServer] Listening on UDP port {} for time queries",
            TIME_SERVER_PORT
        );

        Ok(TimeServer { socket })
    }

    /// Handle pending time query requests.
    ///
    /// This is designed to be called from the main sync loop. It processes
    /// all pending requests without blocking.
    pub fn handle_requests(&self, status: &Arc<RwLock<SyncStatus>>) {
        let mut buf = [0u8; REQUEST_BUF_SIZE];

        // Process all pending requests (non-blocking)
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((size, src)) => {
                    if size >= REQUEST_SIZE {
                        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                        if magic == REQUEST_MAGIC || magic == REQUEST_MAGIC_EXT {
                            let request_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

                            // Read status (handle poisoned lock gracefully)
                            let sync_status = match status.read() {
                                Ok(guard) => guard.clone(),
                                Err(e) => {
                                    warn!("[TimeServer] Status lock poisoned: {}", e);
                                    continue;
                                }
                            };

                            let response = if magic == REQUEST_MAGIC_EXT {
                                build_response_ext(request_id, &sync_status)
                            } else {
                                build_response(request_id, &sync_status).to_vec()
                            };
                            if let Err(e) = self.socket.send_to(&response, src) {
                                debug!("[TimeServer] Failed to send response to {}: {}", src, e);
                            } else {
                                debug!("[TimeServer] Responded to {}", src);
                            }
                        } else {
                            debug!(
                                "[TimeServer] Ignoring packet with invalid magic 0x{:08X} from {}",
                                magic, src
                            );
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No more pending requests
                    break;
                }
                Err(e) => {
                    error!("[TimeServer] Socket error: {}", e);
                    break;
                }
            }
        }
    }
}

/// Build a time query response packet.
fn build_response(request_id: u32, status: &SyncStatus) -> [u8; RESPONSE_SIZE] {
    let mut resp = [0u8; RESPONSE_SIZE];

    // [0-3] Response magic
    resp[0..4].copy_from_slice(&RESPONSE_MAGIC.to_be_bytes());

    // [4-7] Request ID (echo back)
    resp[4..8].copy_from_slice(&request_id.to_be_bytes());

    // [8-15] System time (UTC nanoseconds since Unix epoch)
    let system_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    resp[8..16].copy_from_slice(&system_ns.to_be_bytes());

    // [16-23] Monotonic counter (platform-specific)
    let monotonic = get_monotonic_counter();
    resp[16..24].copy_from_slice(&monotonic.to_be_bytes());

    // [24-31] PTP offset (nanoseconds) - use offset_ns from status
    let ptp_offset_ns = status.offset_ns;
    resp[24..32].copy_from_slice(&ptp_offset_ns.to_be_bytes());

    // [32-35] Drift rate (PPM × 1000)
    let drift_scaled = (status.smoothed_rate_ppm * 1000.0) as i32;
    resp[32..36].copy_from_slice(&drift_scaled.to_be_bytes());

    // [36-39] Frequency adjustment (PPM × 1000)
    let adj_scaled = (status.drift_ppm * 1000.0) as i32;
    resp[36..40].copy_from_slice(&adj_scaled.to_be_bytes());

    // [40] Mode
    resp[40] = match status.mode.as_str() {
        "ACQ" => 1,
        "PROD" => 2,
        "LOCK" => 3,
        "NANO" => 4,
        "NTP-only" => 5,
        _ => 0,
    };

    // [41] Is locked
    resp[41] = if status.is_locked { 1 } else { 0 };

    // [42-47] Grandmaster UUID
    if let Some(uuid) = status.gm_uuid {
        resp[42..48].copy_from_slice(&uuid);
    }

    // [48-55] Monotonic frequency (ticks per second)
    let mono_freq = get_monotonic_frequency();
    resp[48..56].copy_from_slice(&mono_freq.to_be_bytes());

    // [56-59] NTP offset (microseconds, i32)
    let ntp_offset = status.ntp_offset_us as i32;
    resp[56..60].copy_from_slice(&ntp_offset.to_be_bytes());

    // [60-61] Accumulated phase drift (microseconds, i16) — clamped to i16 range
    let phase_clamped = status.accumulated_phase_us.round() as i64;
    let phase_i16 = phase_clamped.clamp(i16::MIN as i64, i16::MAX as i64) as i16;
    resp[60..62].copy_from_slice(&phase_i16.to_be_bytes());

    // [62] Flags byte: bit 0 = ntp_failed, bit 1 = settled
    let mut flags: u8 = 0;
    if status.ntp_failed {
        flags |= 0x01;
    }
    if status.settled {
        flags |= 0x02;
    }
    resp[62] = flags;

    // [63] Reserved (already zero)

    resp
}

/// dantesync#88 — the date-offset extension this node publishes, from its status. `None` until
/// the node has a date-offset state (not PTP-phase-locked yet, or the legacy discipline). The
/// AUTHORITY flag is set only on the master — a follower mirrors its state for observability but
/// must never be adopted by anyone.
///
/// `status.date_offset_ns` is the `D` IN EFFECT; while a coordinated step is scheduled
/// (`date_step_pending_ns`), the published offset is the one it will take — `D + step` — with
/// `date_offset_effective_ptp_ns` (the step's future instant). The controller writes the anchor
/// and the pending step in ONE status update, so a reader never sees the step counted twice.
fn date_extension_from_status(status: &SyncStatus, now_wall_ns: i64) -> Option<DateExtension> {
    let in_effect = status.date_offset_ns?;
    Some(DateExtension {
        version: crate::date_offset::EXT_VERSION,
        authority: status.date_authority == "master",
        gm_uuid: status.date_offset_gm_uuid?,
        // This node's PTP "now" from its D IN EFFECT (never the published, possibly pending D).
        now_ptp_ns: now_wall_ns.wrapping_sub(in_effect),
        announce: DateAnnounce {
            date_offset_ns: in_effect.wrapping_add(status.date_step_pending_ns.unwrap_or(0)),
            effective_ptp_ns: status.date_offset_effective_ptp_ns?,
            seq: status.date_offset_seq?,
        },
    })
}

/// dantesync#88 — the reply to a `"DSYX"` request: the unchanged 64-byte base, then the
/// extension when this node has a date-offset state (else just the base).
fn build_response_ext(request_id: u32, status: &SyncStatus) -> Vec<u8> {
    let mut out = build_response(request_id, status).to_vec();
    let now_wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    if let Some(ext) = date_extension_from_status(status, now_wall) {
        out.extend_from_slice(&encode_extension(&ext));
    }
    out
}

/// dantesync#88 — a `"DSYX"` request (8 bytes, same layout as `"DSYN"`).
pub fn build_ext_request(request_id: u32) -> [u8; REQUEST_SIZE] {
    let mut req = [0u8; REQUEST_SIZE];
    req[0..4].copy_from_slice(&REQUEST_MAGIC_EXT.to_be_bytes());
    req[4..8].copy_from_slice(&request_id.to_be_bytes());
    req
}

/// dantesync#88 — what a follower learns from one reply of its NTP master.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuthorityReply {
    /// Increments on every reply the poller stores, so a consumer acts on each reply once.
    pub serial: u64,
    /// The replying node's grandmaster UUID (base bytes 42-47); `None` when it has none. The
    /// published `D` is only meaningful in THIS grandmaster's PTP time base.
    pub gm_uuid: Option<[u8; 6]>,
    /// The replying node's PTP lock (base byte 41).
    pub is_locked: bool,
    /// This node's wall clock when the reply arrived, ns. With the extension's `now_ptp_ns` it
    /// places both nodes in a PTP time base, so a reply from another base is never adopted
    /// (`crate::date_offset::same_time_base`).
    pub received_wall_ns: i64,
    /// The date-offset extension; `None` from an older server or a node without date state.
    pub ext: Option<DateExtension>,
    /// When the reply arrived (monotonic).
    pub received: Instant,
}

/// dantesync#88 — parse a reply to `build_ext_request(request_id)`. `None` for a short packet,
/// a wrong magic, or a stale reply to a different request.
pub fn parse_reply(
    buf: &[u8],
    request_id: u32,
    received: Instant,
    received_wall_ns: i64,
) -> Option<AuthorityReply> {
    if buf.len() < RESPONSE_SIZE {
        return None;
    }
    if u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) != RESPONSE_MAGIC {
        return None;
    }
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != request_id {
        return None;
    }
    let mut uuid = [0u8; 6];
    uuid.copy_from_slice(&buf[42..48]);
    Some(AuthorityReply {
        serial: 0,
        gm_uuid: if uuid == [0u8; 6] { None } else { Some(uuid) },
        is_locked: buf[41] != 0,
        received_wall_ns,
        ext: decode_extension(&buf[RESPONSE_SIZE..]),
        received,
    })
}

/// dantesync#88 — where the controller reads the latest reply of the date-offset authority.
/// Boxed in the controller so a test injects a scripted authority; the real one is
/// [`UdpAuthorityPoller`].
pub trait DateAuthoritySource: Send {
    fn latest(&self) -> Option<AuthorityReply>;
}

/// No authority at all (the NTP master itself, and the default before `main` wires a poller).
pub struct NoAuthority;

impl DateAuthoritySource for NoAuthority {
    fn latest(&self) -> Option<AuthorityReply> {
        None
    }
}

/// Poll cadence of the authority. The shortest announce lead is 5 s, so every follower gets
/// several chances to hear a pending step before its instant.
pub const AUTHORITY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often the poller re-resolves the authority's host name.
const AUTHORITY_RESOLVE_INTERVAL: Duration = Duration::from_secs(60);

/// How long one poll waits for its reply.
const AUTHORITY_REPLY_TIMEOUT: Duration = Duration::from_millis(500);

/// dantesync#88 — polls the NTP master's 31900 with `"DSYX"` once per second on its own thread
/// and keeps the latest valid reply. DNS resolution and socket waits happen on that thread, so
/// the sync loop only ever takes a short mutex.
pub struct UdpAuthorityPoller {
    latest: Arc<Mutex<Option<AuthorityReply>>>,
}

impl UdpAuthorityPoller {
    /// Start polling `host` (the configured NTP server — the fleet's master) until `running`
    /// clears.
    pub fn spawn(host: String, running: Arc<AtomicBool>) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let shared = latest.clone();
        std::thread::spawn(move || poll_loop(host, running, shared));
        UdpAuthorityPoller { latest }
    }
}

impl DateAuthoritySource for UdpAuthorityPoller {
    fn latest(&self) -> Option<AuthorityReply> {
        self.latest.lock().ok().and_then(|g| *g)
    }
}

fn poll_loop(host: String, running: Arc<AtomicBool>, shared: Arc<Mutex<Option<AuthorityReply>>>) {
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            error!(
                "[DATE] authority poller: cannot bind a UDP socket: {} — no date authority",
                e
            );
            return;
        }
    };
    if let Err(e) = socket.set_read_timeout(Some(AUTHORITY_REPLY_TIMEOUT)) {
        error!(
            "[DATE] authority poller: cannot set the read timeout: {} — no date authority",
            e
        );
        return;
    }
    info!(
        "[DATE] polling the date-offset authority at {}:{} every {:?}",
        host, TIME_SERVER_PORT, AUTHORITY_POLL_INTERVAL
    );
    // An unpredictable request-id start: with the source-address check below, a stray or spoofed
    // DSYR datagram from anywhere else on the LAN is never taken for the authority's reply.
    let mut request_id: u32 = uuid::Uuid::new_v4().as_u128() as u32;
    let mut serial: u64 = 0;
    let mut had_ext: Option<bool> = None;
    let mut buf = [0u8; 256];
    let mut target: Option<std::net::SocketAddr> = None;
    let mut warned_foreign_src = false;
    let mut resolved_at: Option<Instant> = None;
    while running.load(Ordering::SeqCst) {
        let started = Instant::now();
        request_id = request_id.wrapping_add(1);
        // Resolve once a minute (and after a failure), not every second.
        if target.is_none()
            || match resolved_at {
                None => true,
                Some(t) => t.elapsed() >= AUTHORITY_RESOLVE_INTERVAL,
            }
        {
            target = (host.as_str(), TIME_SERVER_PORT)
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.find(|a| a.is_ipv4()));
            resolved_at = Some(Instant::now());
        }
        match target {
            None => debug!("[DATE] authority poller: cannot resolve {}", host),
            Some(addr) => {
                if let Err(e) = socket.send_to(&build_ext_request(request_id), addr) {
                    debug!("[DATE] authority poller: send to {} failed: {}", addr, e);
                } else {
                    // Drain until the reply to THIS request (a late reply to an earlier one is
                    // skipped by the request-id check) or the timeout.
                    while let Ok((n, src)) = socket.recv_from(&mut buf) {
                        if src != addr {
                            // Logged once at info: on a multi-homed master that answers from
                            // another address this is the whole diagnosis.
                            if !warned_foreign_src {
                                info!(
                                    "[DATE] authority poller: ignoring replies from {} (polling \
                                     {}) — only the polled address is trusted",
                                    src, addr
                                );
                                warned_foreign_src = true;
                            }
                            continue;
                        }
                        let received_wall = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_nanos() as i64)
                            .unwrap_or(0);
                        if let Some(mut reply) =
                            parse_reply(&buf[..n], request_id, Instant::now(), received_wall)
                        {
                            serial += 1;
                            reply.serial = serial;
                            let has_ext = reply.ext.is_some_and(|e| e.authority);
                            if had_ext != Some(has_ext) {
                                if has_ext {
                                    info!(
                                        "[DATE] {} publishes the fleet date offset (authority)",
                                        host
                                    );
                                } else {
                                    info!(
                                        "[DATE] {} answers but publishes no date-offset authority \
                                         (older dantesync or not phase-locked) — keeping the local date path",
                                        host
                                    );
                                }
                                had_ext = Some(has_ext);
                            }
                            if let Ok(mut g) = shared.lock() {
                                *g = Some(reply);
                            }
                            break;
                        }
                    }
                }
            }
        }
        let spent = started.elapsed();
        if spent < AUTHORITY_POLL_INTERVAL {
            std::thread::sleep(AUTHORITY_POLL_INTERVAL - spent);
        }
    }
}

/// Get the monotonic counter value (platform-specific).
///
/// - Windows: QueryPerformanceCounter (QPC)
/// - Linux: CLOCK_MONOTONIC_RAW in nanoseconds
#[cfg(unix)]
fn get_monotonic_counter() -> u64 {
    use libc::{clock_gettime, timespec, CLOCK_MONOTONIC_RAW};
    let mut ts = timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        clock_gettime(CLOCK_MONOTONIC_RAW, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

#[cfg(windows)]
fn get_monotonic_counter() -> u64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut counter: i64 = 0;
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
    }
    counter as u64
}

/// Get the monotonic counter frequency (ticks per second).
///
/// - Windows: QueryPerformanceFrequency
/// - Linux: 1,000,000,000 (nanoseconds)
#[cfg(unix)]
fn get_monotonic_frequency() -> u64 {
    // CLOCK_MONOTONIC_RAW returns nanoseconds, so frequency is 10^9
    1_000_000_000
}

#[cfg(windows)]
fn get_monotonic_frequency() -> u64 {
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    let mut freq: i64 = 0;
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    freq as u64
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_magic() {
        let bytes = REQUEST_MAGIC.to_be_bytes();
        assert_eq!(&bytes, b"DSYN");
    }

    #[test]
    fn test_response_magic() {
        let bytes = RESPONSE_MAGIC.to_be_bytes();
        assert_eq!(&bytes, b"DSYR");
    }

    #[test]
    fn test_build_response_format() {
        let status = SyncStatus::default();
        let request_id = 0x12345678u32;
        let response = build_response(request_id, &status);

        // Check magic
        let magic = u32::from_be_bytes([response[0], response[1], response[2], response[3]]);
        assert_eq!(magic, RESPONSE_MAGIC);

        // Check request ID echo
        let echo_id = u32::from_be_bytes([response[4], response[5], response[6], response[7]]);
        assert_eq!(echo_id, request_id);

        // Check system time is reasonable (after 2020)
        let system_ns = u64::from_be_bytes([
            response[8],
            response[9],
            response[10],
            response[11],
            response[12],
            response[13],
            response[14],
            response[15],
        ]);
        let year_2020_ns = 1577836800u64 * 1_000_000_000; // 2020-01-01 in ns
        assert!(system_ns > year_2020_ns, "System time should be after 2020");

        // Check monotonic counter is non-zero
        let mono = u64::from_be_bytes([
            response[16],
            response[17],
            response[18],
            response[19],
            response[20],
            response[21],
            response[22],
            response[23],
        ]);
        assert!(mono > 0, "Monotonic counter should be non-zero");

        // Check monotonic frequency is non-zero
        let freq = u64::from_be_bytes([
            response[48],
            response[49],
            response[50],
            response[51],
            response[52],
            response[53],
            response[54],
            response[55],
        ]);
        assert!(freq > 0, "Monotonic frequency should be non-zero");
    }

    #[test]
    fn test_build_response_with_status() {
        let mut status = SyncStatus::default();
        status.offset_ns = -12345;
        status.smoothed_rate_ppm = 1.5;
        status.drift_ppm = -0.75;
        status.mode = "LOCK".to_string();
        status.is_locked = true;
        status.gm_uuid = Some([0x00, 0x1D, 0xC1, 0xAB, 0xCD, 0xEF]);

        let response = build_response(42, &status);

        // Check PTP offset
        let offset = i64::from_be_bytes([
            response[24],
            response[25],
            response[26],
            response[27],
            response[28],
            response[29],
            response[30],
            response[31],
        ]);
        assert_eq!(offset, -12345);

        // Check drift rate (1.5 * 1000 = 1500)
        let drift = i32::from_be_bytes([response[32], response[33], response[34], response[35]]);
        assert_eq!(drift, 1500);

        // Check frequency adjustment (-0.75 * 1000 = -750)
        let adj = i32::from_be_bytes([response[36], response[37], response[38], response[39]]);
        assert_eq!(adj, -750);

        // Check mode (LOCK = 3)
        assert_eq!(response[40], 3);

        // Check is_locked
        assert_eq!(response[41], 1);

        // Check GM UUID
        assert_eq!(&response[42..48], &[0x00, 0x1D, 0xC1, 0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn test_mode_encoding() {
        let modes = [
            ("", 0),
            ("ACQ", 1),
            ("PROD", 2),
            ("LOCK", 3),
            ("NANO", 4),
            ("NTP-only", 5),
        ];

        for (mode_str, expected) in modes {
            let mut status = SyncStatus::default();
            status.mode = mode_str.to_string();
            let response = build_response(0, &status);
            assert_eq!(
                response[40], expected,
                "Mode '{}' should encode to {}",
                mode_str, expected
            );
        }
    }

    #[test]
    fn test_monotonic_counter_increases() {
        let c1 = get_monotonic_counter();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let c2 = get_monotonic_counter();
        assert!(c2 > c1, "Monotonic counter should increase over time");
    }

    #[test]
    fn test_monotonic_frequency_valid() {
        let freq = get_monotonic_frequency();
        // Should be at least 1MHz (reasonable for any modern system)
        assert!(
            freq >= 1_000_000,
            "Monotonic frequency should be at least 1MHz"
        );
        // On Linux it's exactly 10^9 (nanoseconds)
        #[cfg(unix)]
        assert_eq!(freq, 1_000_000_000);
    }

    #[test]
    fn test_response_size() {
        let status = SyncStatus::default();
        let response = build_response(0, &status);
        assert_eq!(response.len(), RESPONSE_SIZE);
    }

    #[test]
    fn test_time_server_port_constant() {
        assert_eq!(TIME_SERVER_PORT, 31900);
    }

    #[test]
    fn test_build_response_ntp_fields() {
        let mut status = SyncStatus::default();
        status.ntp_offset_us = 1234;
        status.accumulated_phase_us = -567.8;
        status.ntp_failed = false;
        status.settled = true;

        let response = build_response(0, &status);

        // [56-59] NTP offset (i32)
        let ntp_off = i32::from_be_bytes([response[56], response[57], response[58], response[59]]);
        assert_eq!(ntp_off, 1234);

        // [60-61] Accumulated phase (i16, rounded)
        let phase = i16::from_be_bytes([response[60], response[61]]);
        assert_eq!(phase, -568); // -567.8 rounds to -568

        // [62] Flags: bit 0 = ntp_failed (0), bit 1 = settled (1) = 0b10 = 2
        assert_eq!(response[62], 0x02);

        // [63] Reserved
        assert_eq!(response[63], 0);
    }

    #[test]
    fn test_build_response_ntp_failed_flag() {
        let mut status = SyncStatus::default();
        status.ntp_failed = true;
        status.settled = false;

        let response = build_response(0, &status);
        // bit 0 = ntp_failed (1), bit 1 = settled (0) = 0b01 = 1
        assert_eq!(response[62], 0x01);
    }

    #[test]
    fn test_build_response_both_flags() {
        let mut status = SyncStatus::default();
        status.ntp_failed = true;
        status.settled = true;

        let response = build_response(0, &status);
        // bit 0 = ntp_failed (1), bit 1 = settled (1) = 0b11 = 3
        assert_eq!(response[62], 0x03);
    }

    #[test]
    fn test_build_response_ntp_offset_negative() {
        let mut status = SyncStatus::default();
        status.ntp_offset_us = -42000;

        let response = build_response(0, &status);
        let ntp_off = i32::from_be_bytes([response[56], response[57], response[58], response[59]]);
        assert_eq!(ntp_off, -42000);
    }

    // ---- dantesync#88: the versioned date-offset extension ------------------------------

    fn master_status() -> SyncStatus {
        let mut status = SyncStatus::default();
        status.is_locked = true;
        status.mode = "LOCK".to_string();
        status.gm_uuid = Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
        status.ntp_offset_us = -1234;
        status.date_authority = "master".to_string();
        status.date_offset_ns = Some(1_790_000_000_000_000_000);
        status.date_offset_effective_ptp_ns = Some(12_345_000_000_000);
        status.date_offset_seq = Some(3);
        status.date_offset_gm_uuid = Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
        status
    }

    #[test]
    fn test_ext_request_magic() {
        assert_eq!(&REQUEST_MAGIC_EXT.to_be_bytes(), b"DSYX");
        let req = build_ext_request(0xDEADBEEF);
        assert_eq!(&req[0..4], b"DSYX");
        assert_eq!(&req[4..8], &0xDEADBEEFu32.to_be_bytes());
    }

    #[test]
    fn the_extended_reply_is_the_same_base_plus_the_extension_88() {
        let status = master_status();
        let ext_reply = build_response_ext(7, &status);
        assert_eq!(
            ext_reply.len(),
            RESPONSE_SIZE + crate::date_offset::EXT_SIZE
        );
        let base = build_response(7, &status);
        // Every base field an old client reads is identical in the extended reply, except the
        // two clock READINGS (system time [8-15], monotonic counter [16-23]) which are sampled
        // anew by each call.
        assert_eq!(&ext_reply[0..8], &base[0..8]);
        assert_eq!(&ext_reply[24..RESPONSE_SIZE], &base[24..RESPONSE_SIZE]);
        let ext = decode_extension(&ext_reply[RESPONSE_SIZE..]).expect("extension present");
        assert!(ext.authority);
        assert_eq!(ext.announce.date_offset_ns, 1_790_000_000_000_000_000);
        assert_eq!(ext.announce.effective_ptp_ns, 12_345_000_000_000);
        assert_eq!(ext.announce.seq, 3);
    }

    #[test]
    fn an_old_client_reading_only_64_bytes_of_the_extended_reply_decodes_the_same_fields_88() {
        // Older-client compatibility of the payload itself: a parser that knows only the base
        // layout and reads the first 64 bytes sees exactly the fields it always saw.
        let status = master_status();
        let reply = build_response_ext(99, &status);
        let old_view = &reply[..RESPONSE_SIZE];
        assert_eq!(&old_view[0..4], b"DSYR");
        assert_eq!(
            u32::from_be_bytes([old_view[4], old_view[5], old_view[6], old_view[7]]),
            99
        );
        assert_eq!(old_view[40], 3, "mode LOCK");
        assert_eq!(old_view[41], 1, "is_locked");
        assert_eq!(&old_view[42..48], &[0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]);
        assert_eq!(
            i32::from_be_bytes([old_view[56], old_view[57], old_view[58], old_view[59]]),
            -1234
        );
    }

    #[test]
    fn a_pending_step_is_published_as_the_offset_it_will_take_88() {
        let mut status = master_status();
        status.date_step_pending_ns = Some(-51_000_000);
        status.date_offset_effective_ptp_ns = Some(12_350_000_000_000);
        status.date_offset_seq = Some(4);
        let ext = date_extension_from_status(&status, 1_790_000_100_000_000_000).unwrap();
        assert_eq!(
            ext.announce.date_offset_ns,
            1_790_000_000_000_000_000 - 51_000_000
        );
        assert_eq!(
            ext.now_ptp_ns, 100_000_000_000,
            "PTP now from the D in effect, not from the pending one"
        );
        assert_eq!(ext.announce.effective_ptp_ns, 12_350_000_000_000);
        assert_eq!(ext.announce.seq, 4);
    }

    #[test]
    fn no_extension_is_published_without_the_anchor_grandmaster_88() {
        // The controller clears the anchor GM while a re-anchor is pending: no D may be
        // published in a time base nobody can identify.
        let mut status = master_status();
        status.date_offset_gm_uuid = None;
        assert_eq!(build_response_ext(1, &status).len(), RESPONSE_SIZE);
    }

    #[test]
    fn a_node_without_date_state_answers_dsyx_with_the_base_only_88() {
        let reply = build_response_ext(1, &SyncStatus::default());
        assert_eq!(reply.len(), RESPONSE_SIZE);
        let parsed = parse_reply(&reply, 1, Instant::now(), 0).expect("valid base reply");
        assert_eq!(parsed.ext, None);
    }

    #[test]
    fn a_follower_mirrors_its_state_without_the_authority_flag_88() {
        let mut status = master_status();
        status.date_authority = "follower".to_string();
        let reply = build_response_ext(1, &status);
        let parsed = parse_reply(&reply, 1, Instant::now(), 0).unwrap();
        assert!(
            !parsed.ext.unwrap().authority,
            "only the master is an authority"
        );
    }

    #[test]
    fn parse_reply_reads_gm_lock_and_extension_and_rejects_strangers_88() {
        let status = master_status();
        let reply = build_response_ext(42, &status);
        let t = Instant::now();
        let parsed = parse_reply(&reply, 42, t, 0).expect("valid");
        assert_eq!(parsed.gm_uuid, Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]));
        assert!(parsed.is_locked);
        assert_eq!(parsed.ext.unwrap().announce.seq, 3);
        assert_eq!(parsed.received, t);
        let base_wall = i64::from_be_bytes(reply[8..16].try_into().unwrap());
        let now_ptp = parsed.ext.unwrap().now_ptp_ns;
        assert!(
            (now_ptp - (base_wall - 1_790_000_000_000_000_000)).abs() < 1_000_000_000,
            "the replier's PTP now = its wall − its D IN EFFECT"
        );
        assert_eq!(
            parsed.ext.unwrap().gm_uuid,
            [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c],
            "the anchor's grandmaster rides in the extension"
        );

        assert_eq!(
            parse_reply(&reply, 41, t, 0),
            None,
            "a reply to another request id"
        );
        assert_eq!(parse_reply(&reply[..63], 42, t, 0), None, "short");
        let mut bad = reply.clone();
        bad[0] = b'X';
        assert_eq!(parse_reply(&bad, 42, t, 0), None, "wrong magic");
        // An older server's 64-byte reply: valid, no authority.
        let old = build_response(42, &status);
        assert_eq!(parse_reply(&old, 42, t, 0).unwrap().ext, None);
        // A node with no grandmaster reports None, not an all-zero UUID.
        let mut no_gm = master_status();
        no_gm.gm_uuid = None;
        assert_eq!(
            parse_reply(&build_response_ext(1, &no_gm), 1, t, 0)
                .unwrap()
                .gm_uuid,
            None
        );
    }

    /// Run the server's non-blocking dispatch until the client has its reply (bounded: 5 s).
    fn serve_until_reply(
        server: &TimeServer,
        status: &Arc<RwLock<SyncStatus>>,
        client: &UdpSocket,
        buf: &mut [u8],
    ) -> usize {
        for _ in 0..250 {
            server.handle_requests(status);
            if let Ok((n, _)) = client.recv_from(buf) {
                return n;
            }
        }
        panic!("no reply from the time server within 5 s");
    }

    #[test]
    fn the_server_answers_dsyn_with_64_bytes_and_dsyx_with_the_extension_over_real_udp_88() {
        // End-to-end over loopback through the real `handle_requests` dispatch. Bind the server
        // on an ephemeral port (not 31900, which a running daemon may hold).
        let server = TimeServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
        };
        server.socket.set_nonblocking(true).unwrap();
        let addr = server.socket.local_addr().unwrap();
        let status = Arc::new(RwLock::new(master_status()));
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();

        let mut old_req = [0u8; 8];
        old_req[0..4].copy_from_slice(b"DSYN");
        old_req[4..8].copy_from_slice(&5u32.to_be_bytes());
        client.send_to(&old_req, addr).unwrap();
        let mut buf = [0u8; 256];
        let n = serve_until_reply(&server, &status, &client, &mut buf);
        assert_eq!(
            n, RESPONSE_SIZE,
            "an old DSYN request gets exactly 64 bytes"
        );

        client.send_to(&build_ext_request(6), addr).unwrap();
        let n = serve_until_reply(&server, &status, &client, &mut buf);
        assert_eq!(n, RESPONSE_SIZE + crate::date_offset::EXT_SIZE);
        let parsed = parse_reply(&buf[..n], 6, Instant::now(), 0).unwrap();
        assert_eq!(
            parsed.ext.unwrap().announce.date_offset_ns,
            1_790_000_000_000_000_000
        );
    }

    #[test]
    fn test_build_response_phase_clamp() {
        let mut status = SyncStatus::default();
        // Exceeds i16 range — should be clamped to i16::MAX
        status.accumulated_phase_us = 50000.0;

        let response = build_response(0, &status);
        let phase = i16::from_be_bytes([response[60], response[61]]);
        assert_eq!(phase, i16::MAX); // 32767
    }
}
