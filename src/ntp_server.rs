//! Minimal NTP/SNTP Server implementation for DanteSync.
//!
//! This module provides a simple NTP server that responds to client queries
//! with the current system time. It's designed for use in Dante audio networks
//! where one machine serves as the time master, disciplined by PTP, and all
//! other machines sync their time via NTP from this master.
//!
//! The server implements RFC 5905 (NTPv4) at a basic level, supporting:
//! - NTPv3 and NTPv4 client requests
//! - Standard 48-byte NTP packet format
//! - Configurable stratum level
//!
//! This is NOT a full-featured NTP server. It's optimized for LAN use where
//! all clients trust this server as the authoritative time source.

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ============================================================================
// NTP PROTOCOL CONSTANTS
// ============================================================================

/// NTP packet size (48 bytes)
const NTP_PACKET_SIZE: usize = 48;

/// NTP epoch offset from Unix epoch (1900-01-01 to 1970-01-01 in seconds)
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

/// LI (Leap Indicator): 0 = no warning
const LI_NO_WARNING: u8 = 0;

/// Mode: 4 = server
const MODE_SERVER: u8 = 4;

/// Mode: 3 = client
const MODE_CLIENT: u8 = 3;

/// Reference ID for local clock (ASCII "LOCL")
const REF_ID_LOCL: u32 = 0x4C4F434C;

// ============================================================================
// NTP SERVER
// ============================================================================

/// Minimal NTP server that responds to client queries with current system time.
///
/// # Usage
/// ```ignore
/// let server = NtpServer::new(123, 3)?;
/// server.run(running_flag)?;
/// ```
pub struct NtpServer {
    socket: UdpSocket,
    stratum: u8,
    /// When we synced from upstream NTP (for reference timestamp)
    reference_time: SystemTime,
}

impl NtpServer {
    /// Create a new NTP server bound to the specified port.
    ///
    /// # Arguments
    /// * `port` - UDP port to listen on (usually 123, requires elevated privileges)
    /// * `stratum` - Stratum level to report (typically 2-4 for LAN servers)
    pub fn new(port: u16, stratum: u8) -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", port);
        let socket = UdpSocket::bind(&bind_addr).map_err(|e| {
            anyhow!(
                "Failed to bind NTP server to {}: {} (hint: port 123 requires root/admin)",
                bind_addr,
                e
            )
        })?;

        // Set non-blocking for graceful shutdown
        socket.set_nonblocking(true)?;

        // Set read timeout for polling
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        info!(
            "[NTP-Server] Listening on {} (stratum {})",
            bind_addr, stratum
        );

        Ok(NtpServer {
            socket,
            stratum,
            reference_time: SystemTime::now(),
        })
    }

    /// Run the NTP server loop until the running flag is cleared.
    pub fn run(&self, running: Arc<AtomicBool>) -> Result<()> {
        let mut buf = [0u8; NTP_PACKET_SIZE];

        while running.load(Ordering::SeqCst) {
            match self.socket.recv_from(&mut buf) {
                Ok((size, src)) => {
                    if size >= NTP_PACKET_SIZE {
                        if let Err(e) = self.handle_request(&buf, src) {
                            warn!("[NTP-Server] Error handling request from {}: {}", src, e);
                        }
                    } else {
                        debug!(
                            "[NTP-Server] Ignoring short packet ({} bytes) from {}",
                            size, src
                        );
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No packet available, continue polling
                    continue;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    // Read timeout, continue polling
                    continue;
                }
                Err(e) => {
                    error!("[NTP-Server] Socket error: {}", e);
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        info!("[NTP-Server] Shutting down");
        Ok(())
    }

    /// Handle a single NTP request and send response.
    fn handle_request(&self, request: &[u8], src: SocketAddr) -> Result<()> {
        // Parse request header
        let li_vn_mode = request[0];
        let version = (li_vn_mode >> 3) & 0x07;
        let mode = li_vn_mode & 0x07;

        // Only respond to client requests (mode 3)
        if mode != MODE_CLIENT {
            debug!(
                "[NTP-Server] Ignoring non-client mode {} from {}",
                mode, src
            );
            return Ok(());
        }

        // Validate version (support v3 and v4)
        if !(3..=4).contains(&version) {
            debug!(
                "[NTP-Server] Ignoring unsupported version {} from {}",
                version, src
            );
            return Ok(());
        }

        // Get timestamps
        let receive_time = SystemTime::now();
        let (recv_secs, recv_frac) = system_time_to_ntp(receive_time);

        // Extract client's transmit timestamp (bytes 40-47 in request)
        // This becomes the originate timestamp in our response
        let originate_ts = &request[40..48];

        // Build response
        let response = self.build_response(version, originate_ts, recv_secs, recv_frac)?;

        // Send response
        self.socket.send_to(&response, src)?;
        debug!("[NTP-Server] Responded to {} (v{})", src, version);

        Ok(())
    }

    /// Build an NTP response packet.
    fn build_response(
        &self,
        version: u8,
        originate_ts: &[u8],
        recv_secs: u32,
        recv_frac: u32,
    ) -> Result<[u8; NTP_PACKET_SIZE]> {
        let mut response = [0u8; NTP_PACKET_SIZE];

        // Byte 0: LI (2 bits) | VN (3 bits) | Mode (3 bits)
        response[0] = (LI_NO_WARNING << 6) | (version << 3) | MODE_SERVER;

        // Byte 1: Stratum
        response[1] = self.stratum;

        // Byte 2: Poll interval (2^6 = 64 seconds typical)
        response[2] = 6;

        // Byte 3: Precision (2^-20 ≈ 1µs, typical for software clock)
        response[3] = 0xEC; // -20 as signed byte

        // Bytes 4-7: Root Delay (0 for local)
        // Already zero

        // Bytes 8-11: Root Dispersion (small value for local)
        response[8] = 0;
        response[9] = 0;
        response[10] = 0;
        response[11] = 16; // ~1ms dispersion

        // Bytes 12-15: Reference ID ("LOCL" for local clock)
        let ref_id = REF_ID_LOCL.to_be_bytes();
        response[12..16].copy_from_slice(&ref_id);

        // Bytes 16-23: Reference Timestamp (when we synced from upstream)
        let (ref_secs, ref_frac) = system_time_to_ntp(self.reference_time);
        response[16..20].copy_from_slice(&ref_secs.to_be_bytes());
        response[20..24].copy_from_slice(&ref_frac.to_be_bytes());

        // Bytes 24-31: Originate Timestamp (copy client's transmit timestamp)
        response[24..32].copy_from_slice(originate_ts);

        // Bytes 32-39: Receive Timestamp (when we received the request)
        response[32..36].copy_from_slice(&recv_secs.to_be_bytes());
        response[36..40].copy_from_slice(&recv_frac.to_be_bytes());

        // Bytes 40-47: Transmit Timestamp (now)
        let transmit_time = SystemTime::now();
        let (tx_secs, tx_frac) = system_time_to_ntp(transmit_time);
        response[40..44].copy_from_slice(&tx_secs.to_be_bytes());
        response[44..48].copy_from_slice(&tx_frac.to_be_bytes());

        Ok(response)
    }

    /// Update the reference timestamp (call after initial NTP sync).
    pub fn set_reference_time(&mut self, time: SystemTime) {
        self.reference_time = time;
    }
}

// ============================================================================
// NTP TIMESTAMP HELPERS
// ============================================================================

/// Convert SystemTime to NTP timestamp (seconds since 1900, fractional seconds).
fn system_time_to_ntp(time: SystemTime) -> (u32, u32) {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let unix_secs = duration.as_secs();
    let ntp_secs = unix_secs + NTP_EPOCH_OFFSET;

    // Fractional part: nanos -> 32-bit fraction
    // frac = nanos * 2^32 / 10^9
    let nanos = duration.subsec_nanos() as u64;
    let frac = ((nanos << 32) / 1_000_000_000) as u32;

    (ntp_secs as u32, frac)
}

/// Convert NTP timestamp to SystemTime.
#[allow(dead_code)]
fn ntp_to_system_time(secs: u32, frac: u32) -> SystemTime {
    let unix_secs = (secs as u64).saturating_sub(NTP_EPOCH_OFFSET);
    // frac * 10^9 / 2^32
    let nanos = ((frac as u64 * 1_000_000_000) >> 32) as u32;
    UNIX_EPOCH + Duration::new(unix_secs, nanos)
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_time_to_ntp_epoch() {
        // Unix epoch (1970-01-01 00:00:00) should be NTP epoch + 70 years
        let unix_epoch = UNIX_EPOCH;
        let (secs, _) = system_time_to_ntp(unix_epoch);
        assert_eq!(secs as u64, NTP_EPOCH_OFFSET);
    }

    #[test]
    fn test_system_time_to_ntp_roundtrip() {
        let original = SystemTime::now();
        let (secs, frac) = system_time_to_ntp(original);
        let recovered = ntp_to_system_time(secs, frac);

        // Should be within 1µs (due to fractional precision)
        let diff = original
            .duration_since(recovered)
            .or_else(|_| recovered.duration_since(original))
            .unwrap_or_default();
        assert!(diff.as_micros() < 10, "Roundtrip error: {:?}", diff);
    }

    #[test]
    fn test_ntp_fractional_conversion() {
        // Test 0.5 seconds
        let time = UNIX_EPOCH + Duration::new(0, 500_000_000);
        let (_, frac) = system_time_to_ntp(time);
        // 0.5 * 2^32 = 2147483648
        assert!(
            (frac as i64 - 2147483648).abs() < 1000,
            "Fractional 0.5s conversion: {}",
            frac
        );
    }

    #[test]
    fn test_ntp_fractional_quarter() {
        // Test 0.25 seconds
        let time = UNIX_EPOCH + Duration::new(0, 250_000_000);
        let (_, frac) = system_time_to_ntp(time);
        // 0.25 * 2^32 = 1073741824
        assert!(
            (frac as i64 - 1073741824).abs() < 1000,
            "Fractional 0.25s conversion: {}",
            frac
        );
    }

    #[test]
    fn test_ntp_packet_constants() {
        assert_eq!(NTP_PACKET_SIZE, 48);
        assert_eq!(NTP_EPOCH_OFFSET, 2_208_988_800);
        assert_eq!(LI_NO_WARNING, 0);
        assert_eq!(MODE_SERVER, 4);
        assert_eq!(MODE_CLIENT, 3);
    }

    #[test]
    fn test_ref_id_locl() {
        // "LOCL" in ASCII
        let bytes = REF_ID_LOCL.to_be_bytes();
        assert_eq!(bytes, [b'L', b'O', b'C', b'L']);
    }

    #[test]
    fn test_build_response_format() {
        // Create a mock server (won't actually bind in tests)
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            reference_time: SystemTime::now(),
        };

        let originate_ts = [0u8; 8];
        let response = server.build_response(4, &originate_ts, 100, 200).unwrap();

        // Check header
        let li_vn_mode = response[0];
        let li = (li_vn_mode >> 6) & 0x03;
        let vn = (li_vn_mode >> 3) & 0x07;
        let mode = li_vn_mode & 0x07;

        assert_eq!(li, LI_NO_WARNING);
        assert_eq!(vn, 4); // Version 4
        assert_eq!(mode, MODE_SERVER);

        // Check stratum
        assert_eq!(response[1], 3);

        // Check reference ID
        assert_eq!(&response[12..16], b"LOCL");
    }

    #[test]
    fn test_response_copies_originate_timestamp() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            reference_time: SystemTime::now(),
        };

        let originate_ts = [1, 2, 3, 4, 5, 6, 7, 8];
        let response = server.build_response(4, &originate_ts, 100, 200).unwrap();

        // Originate timestamp should be at bytes 24-31
        assert_eq!(&response[24..32], &originate_ts);
    }

    #[test]
    fn test_receive_timestamp_in_response() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            reference_time: SystemTime::now(),
        };

        let recv_secs: u32 = 0x12345678;
        let recv_frac: u32 = 0xABCDEF00;
        let response = server
            .build_response(4, &[0; 8], recv_secs, recv_frac)
            .unwrap();

        // Receive timestamp at bytes 32-39
        let recv_secs_out =
            u32::from_be_bytes([response[32], response[33], response[34], response[35]]);
        let recv_frac_out =
            u32::from_be_bytes([response[36], response[37], response[38], response[39]]);

        assert_eq!(recv_secs_out, recv_secs);
        assert_eq!(recv_frac_out, recv_frac);
    }

    #[test]
    fn test_version_3_response() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            reference_time: SystemTime::now(),
        };

        let response = server.build_response(3, &[0; 8], 100, 200).unwrap();

        let vn = (response[0] >> 3) & 0x07;
        assert_eq!(vn, 3, "Response should match client's version");
    }

    #[test]
    fn test_set_reference_time() {
        let mut server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            reference_time: UNIX_EPOCH,
        };

        let new_time = SystemTime::now();
        server.set_reference_time(new_time);

        // The reference time should be updated
        let diff = server
            .reference_time
            .duration_since(new_time)
            .or_else(|_| new_time.duration_since(server.reference_time))
            .unwrap_or_default();
        assert!(diff.as_millis() < 10);
    }
}
