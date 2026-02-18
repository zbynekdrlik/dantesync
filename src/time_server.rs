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
//! - `[56-63]` Reserved (zeros)

use crate::status::SyncStatus;
use anyhow::Result;
use log::{debug, error, info, warn};
use std::net::UdpSocket;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// UDP port for time query server
pub const TIME_SERVER_PORT: u16 = 31900;

/// Request magic bytes: "DSYN"
const REQUEST_MAGIC: u32 = 0x4453594E;

/// Response magic bytes: "DSYR"
const RESPONSE_MAGIC: u32 = 0x44535952;

/// Minimum request packet size
const REQUEST_SIZE: usize = 8;

/// Response packet size
const RESPONSE_SIZE: usize = 64;

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
        let mut buf = [0u8; REQUEST_SIZE];

        // Process all pending requests (non-blocking)
        loop {
            match self.socket.recv_from(&mut buf) {
                Ok((size, src)) => {
                    if size >= REQUEST_SIZE {
                        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
                        if magic == REQUEST_MAGIC {
                            let request_id = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

                            // Read status (handle poisoned lock gracefully)
                            let sync_status = match status.read() {
                                Ok(guard) => guard.clone(),
                                Err(e) => {
                                    warn!("[TimeServer] Status lock poisoned: {}", e);
                                    continue;
                                }
                            };

                            let response = build_response(request_id, &sync_status);
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

    // [56-63] Reserved (already zero)

    resp
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
}
