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
//! A request whose magic is `"DSYX"` (0x44535958) instead of `"DSYN"`, zero-padded to 64 bytes
//! (so the reply never amplifies it; a shorter one is ignored), asks for the reply's versioned
//! extension: the same 64-byte base, then — when this node has a date-offset state —
//! the [`crate::date_offset`] extension (`[64]` version, `[65]` flags with bit 0 = the fleet's
//! date-offset AUTHORITY, `[68-75]` `date_offset_ns`, `[76-83]` `effective_ptp_ns`, `[84-87]`
//! `seq`, `[88-93]` the anchor's grandmaster UUID, `[96-103]` the replier's PTP "now"). See
//! `date_offset::EXT_SIZE` for the authoritative layout. Compatibility, both directions:
//!
//! - an OLD client sends `"DSYN"` and gets the byte-identical 64-byte reply it always got — it
//!   never sees extra bytes (a 64-byte receive buffer on Windows would otherwise fail the whole
//!   datagram with `WSAEMSGSIZE`, not truncate it);
//! - an OLD server never answers `"DSYX"`, so a new client talking to it simply hears no
//!   authority and keeps its local date fallback. On Linux the old server reads the first 8
//!   bytes and ignores the unknown magic (debug-logged). On WINDOWS its 8-byte receive buffer is
//!   smaller than the padded request, so the read fails (`WSAEMSGSIZE`) and it logs a socket
//!   error per poll: 60 in the poller's first minute, then 2 a minute. Upgrade a master before
//!   its followers (the canary-first fleet order does).
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

/// dantesync#88 — size of a `"DSYX"` request: padded to the base reply's size, so the extended
/// reply is never much larger than the request that asked for it (no amplification).
pub const EXT_REQUEST_SIZE: usize = RESPONSE_SIZE;

/// dantesync#88 — receive buffer for requests: exactly the largest request we accept (a padded
/// `"DSYX"`). A datagram larger than the buffer fails `recv_from` on Windows (`WSAEMSGSIZE`)
/// instead of being truncated, so this must never shrink below [`EXT_REQUEST_SIZE`].
const REQUEST_BUF_SIZE: usize = 64;
const _: () = assert!(REQUEST_BUF_SIZE >= EXT_REQUEST_SIZE);

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
                        if magic == REQUEST_MAGIC_EXT && size < EXT_REQUEST_SIZE {
                            // #88: the extended reply is 104 bytes; answering a short request
                            // would make every spoofed one an amplifier.
                            debug!(
                                "[TimeServer] Ignoring an unpadded DSYX request ({} bytes) from {}",
                                size, src
                            );
                        } else if magic == REQUEST_MAGIC || magic == REQUEST_MAGIC_EXT {
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

/// dantesync#88 — a `"DSYX"` request: the `"DSYN"` layout (magic, request id), zero-padded to
/// [`EXT_REQUEST_SIZE`].
pub fn build_ext_request(request_id: u32) -> [u8; EXT_REQUEST_SIZE] {
    let mut req = [0u8; EXT_REQUEST_SIZE];
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

/// Consecutive unanswered polls after which a poller whose host has NEVER answered slows down:
/// that host does not speak `"DSYX"` (an older dantesync, a public NTP server, a firewall).
pub const AUTHORITY_SILENT_POLLS_BEFORE_BACKOFF: u32 = 60;

/// Poll cadence while backed off. The first reply restores the 1 s cadence for good.
pub const AUTHORITY_BACKOFF_INTERVAL: Duration = Duration::from_secs(30);

/// dantesync#88 — the poller's cadence. 1 s, except for a host that has never answered `"DSYX"`
/// since this poller started: after a minute of silence that one is polled every 30 s. A host that
/// answered once is a real authority, and a silence from it (a reboot, a network blip) never slows
/// the poll: an announce it makes when it is back is at most 5 s ahead of its instant, and a
/// follower polling every 30 s would hear it too late.
#[derive(Debug, Default)]
pub struct PollBackoff {
    silent: u32,
    answered_once: bool,
}

impl PollBackoff {
    /// A reply to this poll arrived.
    pub fn on_reply(&mut self) {
        self.silent = 0;
        self.answered_once = true;
    }

    /// This poll went unanswered.
    pub fn on_silence(&mut self) {
        self.silent = self.silent.saturating_add(1);
    }

    /// True once the poller runs at the slow cadence.
    pub fn backed_off(&self) -> bool {
        !self.answered_once && self.silent >= AUTHORITY_SILENT_POLLS_BEFORE_BACKOFF
    }

    /// How long until the next poll.
    pub fn interval(&self) -> Duration {
        if self.backed_off() {
            AUTHORITY_BACKOFF_INTERVAL
        } else {
            AUTHORITY_POLL_INTERVAL
        }
    }
}

/// dantesync#88 — polls the NTP master's 31900 with `"DSYX"` once per second on its own thread
/// (every 30 s while a host that never answered stays silent, see [`PollBackoff`]) and keeps the
/// latest valid reply. DNS resolution and socket waits happen on that thread, so
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
    let mut backoff = PollBackoff::default();
    while running.load(Ordering::SeqCst) {
        let started = Instant::now();
        let mut answered = false;
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
                            answered = true;
                            break;
                        }
                    }
                }
            }
        }
        let was_backed_off = backoff.backed_off();
        if answered {
            backoff.on_reply();
        } else {
            backoff.on_silence();
        }
        if backoff.backed_off() != was_backed_off {
            if was_backed_off {
                info!(
                    "[DATE] {} answers DSYX now — polling every {:?}",
                    host, AUTHORITY_POLL_INTERVAL
                );
            } else {
                info!(
                    "[DATE] no DSYX reply from {} for {} polls (an older dantesync, a public NTP \
                     server or a firewall?) — polling every {:?} until it answers",
                    host, AUTHORITY_SILENT_POLLS_BEFORE_BACKOFF, AUTHORITY_BACKOFF_INTERVAL
                );
            }
        }
        let interval = backoff.interval();
        let spent = started.elapsed();
        if spent < interval {
            std::thread::sleep(interval - spent);
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
mod tests;
