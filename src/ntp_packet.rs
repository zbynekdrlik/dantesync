//! Pure NTP (RFC 5905 / SNTP) wire-format helpers + offset/RTT math.
//!
//! # Why this file exists (dantesync#53)
//!
//! The Windows NTP client's offset scatters by tens of milliseconds even
//! while the PTP servo reports locked. Root cause: `NtpClient::measure_once()`
//! (`src/ntp.rs`) takes t1/t4 as plain `SystemTime::now()` calls around a
//! blocking `std::net::UdpSocket` round trip — any scheduling jitter between
//! "the packet actually left/arrived" and "our thread got to read the clock"
//! leaks straight into the measurement. A live canary proved the noise lives
//! INSIDE a single burst of back-to-back measurements (877-21094us spread
//! taken within milliseconds of each other on the same box, same server),
//! so no amount of RTT-selection/filtering across a burst can rescue it —
//! the measurement itself has to change.
//!
//! The fix: capture t1/t4 from the kernel-timestamped Npcap path Windows
//! already uses for PTP (`net_pcap.rs`'s `HostHighPrec` / `KeQuerySystemTimePrecise()`)
//! instead of userspace `SystemTime::now()`. That capture-and-correlate glue
//! (`PcapNtpTransport` in `net_pcap.rs`) needs real Npcap hardware and cannot
//! be unit-tested in CI — so EVERYTHING it depends on is pulled out here,
//! into a module with zero pcap/socket/OS dependency, so it compiles and is
//! fully unit-tested on every platform including plain Linux CI:
//!
//! - NTP <-> Unix epoch timestamp conversion
//! - Building a client (mode 3) request packet
//! - Parsing a server (mode 4) reply packet's t2/t3 fields
//! - The standard offset/round-trip-delay formula from 4 timestamps
//! - Parsing an Ethernet+IPv4+UDP frame as captured by pcap (shared with the
//!   existing PTP Npcap path, which used to duplicate this exact parsing
//!   inline in `net_pcap.rs`'s `recv_packet`)
//!
//! None of this touches I/O, sockets, or the `pcap` crate — it is pure
//! bytes-in/values-out logic, exactly the kind of seam
//! `autonomous-verification.md`/`tdd-workflow.md` ask for when the thing
//! that ACTUALLY talks to hardware cannot be tested without that hardware.

use anyhow::{anyhow, Result};
use std::net::Ipv4Addr;
use std::time::SystemTime;

// ============================================================================
// NTP <-> Unix epoch timestamp conversion
// ============================================================================

/// Seconds between the NTP epoch (1900-01-01 00:00:00 UTC) and the Unix
/// epoch (1970-01-01 00:00:00 UTC).
pub const NTP_UNIX_EPOCH_DELTA_SECS: i64 = 2_208_988_800;

/// Convert an NTP 64-bit fixed-point timestamp (32-bit seconds since 1900 +
/// 32-bit binary fraction of a second) into signed microseconds since the
/// Unix epoch.
///
/// #53 RED: the naive placeholder always returns 0 — any fixture with a
/// non-epoch timestamp fails immediately.
pub fn ntp_timestamp_to_unix_micros(_seconds: u32, _fraction: u32) -> i64 {
    0
}

/// Convert microseconds-since-Unix-epoch back into an NTP (seconds, fraction)
/// pair. Handles pre-1970 values (negative `unix_us`) via Euclidean division
/// so the fractional part always stays non-negative.
///
/// #53 RED: the naive placeholder always returns `(0, 0)` — any non-epoch
/// fixture fails immediately.
pub fn unix_micros_to_ntp_timestamp(_unix_us: i64) -> (u32, u32) {
    (0, 0)
}

/// Convert a `SystemTime` to signed microseconds since the Unix epoch. Never
/// panics on a time before 1970 — this is a diagnostics/measurement path,
/// not something that should ever crash the sync loop over a clock quirk.
pub fn systemtime_to_unix_micros(ts: SystemTime) -> i64 {
    match ts.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

// ============================================================================
// NTP packet build/parse (48-byte RFC 5905 header, mode 3 client / mode 4 server)
// ============================================================================

/// Length of an NTP header with no extension fields.
pub const NTP_PACKET_LEN: usize = 48;

const NTP_VERSION: u8 = 4;
const NTP_MODE_CLIENT: u8 = 3;
const NTP_MODE_SERVER: u8 = 4;
const NTP_MODE_MASK: u8 = 0x07;

/// Build a 48-byte NTP client (mode 3) request packet, embedding
/// `transmit_ts_us` (our own clock — informational only; the server echoes
/// it back as the Origin Timestamp, which a caller COULD use to correlate
/// request/response, though `PcapNtpTransport` matches by capture direction
/// instead) as the packet's own Transmit Timestamp field.
///
/// #53 RED: the naive placeholder returns an all-zero buffer — the LI/VN/Mode
/// byte and the embedded timestamp are both wrong.
pub fn build_client_request(_transmit_ts_us: i64) -> [u8; NTP_PACKET_LEN] {
    [0u8; NTP_PACKET_LEN]
}

/// The two timestamps we need out of a server reply: t2 (server receive) and
/// t3 (server transmit). `origin_ts_us` (echoed t1) is also exposed for a
/// caller that wants request/response correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedReply {
    pub origin_ts_us: i64,
    pub receive_ts_us: i64,
    pub transmit_ts_us: i64,
}

/// Parse an NTP server reply (mode 4) from a UDP payload.
///
/// #53 RED: the naive placeholder accepts ANY buffer (no length check, no
/// mode check) and always returns zeros — it doesn't reject a too-short
/// buffer, doesn't reject a non-server-mode packet, and doesn't extract the
/// real timestamps.
pub fn parse_reply(_buf: &[u8]) -> Result<ParsedReply> {
    Ok(ParsedReply {
        origin_ts_us: 0,
        receive_ts_us: 0,
        transmit_ts_us: 0,
    })
}

// ============================================================================
// Offset / round-trip-delay math (RFC 5905 §8)
// ============================================================================

/// The standard NTP/SNTP offset + round-trip-delay formulas:
/// `offset = ((t2-t1) + (t3-t4)) / 2`, `rtt = (t4-t1) - (t3-t2)`.
/// t1 = our send time, t2 = server receive time, t3 = server transmit time,
/// t4 = our receive time (all signed microseconds since the Unix epoch).
///
/// #53 RED: the naive placeholder always returns `(0, 0)` — any fixture with
/// a genuine offset or non-zero delay fails immediately.
pub fn compute_offset_rtt_us(_t1_us: i64, _t2_us: i64, _t3_us: i64, _t4_us: i64) -> (i64, i64) {
    (0, 0)
}

// ============================================================================
// Ethernet + IPv4 + UDP frame parsing (as captured by pcap)
// ============================================================================

/// Parse an Ethernet(14) + IPv4(20, no options) + UDP(8) frame as captured by
/// pcap: returns (source IPv4, destination port, UDP payload slice).
///
/// Pure — takes raw captured bytes, no `pcap` crate types — so it is
/// testable without a capture device. Shared by the PTP (`net_pcap.rs`) and
/// NTP (dantesync#53) Windows Npcap capture paths, which otherwise
/// duplicated this exact parsing.
///
/// #53 RED: the naive placeholder skips every real check (ethertype,
/// protocol) and always returns `(0.0.0.0, 0, ...)` for anything long enough
/// — a non-IPv4 or non-UDP frame is wrongly accepted, and the real source IP
/// / destination port are never extracted.
pub fn parse_udp_frame(data: &[u8]) -> Option<(Ipv4Addr, u16, &[u8])> {
    const ETH_IP_UDP_HEADER: usize = 42;
    if data.len() < ETH_IP_UDP_HEADER {
        return None;
    }
    Some((Ipv4Addr::UNSPECIFIED, 0, &data[ETH_IP_UDP_HEADER..]))
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- NTP <-> Unix epoch conversion ----

    /// #53 RED: converting the NTP epoch itself (seconds = the delta,
    /// fraction = 0) must land exactly on unix_us = 0.
    #[test]
    fn ntp_timestamp_to_unix_micros_at_ntp_epoch_boundary_is_zero() {
        let unix_us = ntp_timestamp_to_unix_micros(NTP_UNIX_EPOCH_DELTA_SECS as u32, 0);
        assert_eq!(
            unix_us, 0,
            "NTP epoch delta with zero fraction is unix_us=0"
        );
    }

    /// #53 RED: a half-second fraction (2^31 / 2^32 = 0.5) must decode to
    /// exactly 500_000us past the second boundary.
    #[test]
    fn ntp_timestamp_to_unix_micros_half_second_fraction_is_exact() {
        let unix_us =
            ntp_timestamp_to_unix_micros(NTP_UNIX_EPOCH_DELTA_SECS as u32 + 100, 2_147_483_648);
        assert_eq!(
            unix_us, 100_500_000,
            "100s + 0.5s fraction must decode to exactly 100_500_000us"
        );
    }

    /// #53 RED: encoding then decoding the NTP epoch (unix_us=0) and an exact
    /// half-second value must round-trip EXACTLY (both are exactly
    /// representable in 32-bit binary fixed point).
    #[test]
    fn unix_micros_ntp_roundtrip_is_exact_at_whole_and_half_second_boundaries() {
        for unix_us in [0i64, 500_000, 1_000_000, 1_500_000, -500_000] {
            let (secs, frac) = unix_micros_to_ntp_timestamp(unix_us);
            let roundtripped = ntp_timestamp_to_unix_micros(secs, frac);
            assert_eq!(
                roundtripped, unix_us,
                "whole/half-second value {} must round-trip exactly",
                unix_us
            );
        }
    }

    /// #53 RED: an arbitrary microsecond value must round-trip within 1us
    /// (32-bit binary fraction resolution is ~0.233ns, far finer than 1us —
    /// only integer-division rounding in each direction can lose anything).
    #[test]
    fn unix_micros_ntp_roundtrip_is_within_one_microsecond_for_arbitrary_values() {
        for unix_us in [123_456i64, 987_654, 1_753_000_123_456, -42] {
            let (secs, frac) = unix_micros_to_ntp_timestamp(unix_us);
            let roundtripped = ntp_timestamp_to_unix_micros(secs, frac);
            let diff = (roundtripped - unix_us).abs();
            assert!(
                diff <= 1,
                "arbitrary value {} round-tripped to {} (diff {}, expected <=1us)",
                unix_us,
                roundtripped,
                diff
            );
        }
    }

    // ---- Client request build ----

    /// #53 RED: byte 0 must be LI=0/VN=4/Mode=3 = 0x23. The naive stub
    /// returns an all-zero buffer, so byte 0 is 0x00.
    #[test]
    fn build_client_request_sets_li_vn_mode_byte() {
        let packet = build_client_request(0);
        assert_eq!(
            packet[0], 0x23,
            "LI=0,VN=4,Mode=3 must encode to byte 0x23, got {:#04x}",
            packet[0]
        );
    }

    /// #53 RED: the Transmit Timestamp field (bytes 40..48) must decode back
    /// to the value passed in. The naive stub never writes it.
    #[test]
    fn build_client_request_encodes_transmit_timestamp() {
        let packet = build_client_request(100_500_000);
        let secs = u32::from_be_bytes(packet[40..44].try_into().unwrap());
        let frac = u32::from_be_bytes(packet[44..48].try_into().unwrap());
        let decoded = ntp_timestamp_to_unix_micros(secs, frac);
        assert_eq!(
            decoded, 100_500_000,
            "embedded Transmit Timestamp must decode back to the input value"
        );
    }

    // ---- Server reply parse ----

    /// Build a fake 48-byte NTP server (mode 4) reply with known origin
    /// (bytes 24..32), receive (32..40), and transmit (40..48) timestamps.
    fn fake_server_reply(
        origin_us: i64,
        receive_us: i64,
        transmit_us: i64,
    ) -> [u8; NTP_PACKET_LEN] {
        let mut buf = [0u8; NTP_PACKET_LEN];
        buf[0] = (NTP_VERSION << 3) | NTP_MODE_SERVER;
        let write_ts = |buf: &mut [u8; NTP_PACKET_LEN], off: usize, us: i64| {
            let (secs, frac) = unix_micros_to_ntp_timestamp(us);
            buf[off..off + 4].copy_from_slice(&secs.to_be_bytes());
            buf[off + 4..off + 8].copy_from_slice(&frac.to_be_bytes());
        };
        write_ts(&mut buf, 24, origin_us);
        write_ts(&mut buf, 32, receive_us);
        write_ts(&mut buf, 40, transmit_us);
        buf
    }

    /// #53 RED: a well-formed server reply must yield the exact origin/
    /// receive/transmit values that were encoded into it.
    #[test]
    fn parse_reply_extracts_origin_receive_and_transmit_timestamps() {
        let buf = fake_server_reply(1_000_000, 1_010_000, 1_020_000);
        let reply = parse_reply(&buf).expect("well-formed server reply must parse");
        assert_eq!(reply.origin_ts_us, 1_000_000);
        assert_eq!(reply.receive_ts_us, 1_010_000);
        assert_eq!(reply.transmit_ts_us, 1_020_000);
    }

    /// #53 RED: a buffer shorter than the 48-byte NTP header must be
    /// rejected, not silently accepted with garbage/zero fields.
    #[test]
    fn parse_reply_rejects_too_short_buffer() {
        let buf = [0u8; 10];
        assert!(
            parse_reply(&buf).is_err(),
            "a 10-byte buffer is not a valid NTP packet and must be rejected"
        );
    }

    /// #53 RED: a packet with the CLIENT mode (3) — e.g. our own request
    /// looped back, or a malformed/spoofed packet — must be rejected as a
    /// reply, not parsed as if it were a real server response.
    #[test]
    fn parse_reply_rejects_non_server_mode() {
        let mut buf = fake_server_reply(1, 2, 3);
        buf[0] = (NTP_VERSION << 3) | NTP_MODE_CLIENT;
        assert!(
            parse_reply(&buf).is_err(),
            "a mode-3 (client) packet must never be accepted as a server reply"
        );
    }

    // ---- Offset / RTT math ----

    /// #53 RED: a perfectly symmetric network path (equal delay each way)
    /// must yield offset=0 — any true clock difference cancels out of the
    /// formula only when the path truly is symmetric, which this fixture is
    /// by construction.
    #[test]
    fn compute_offset_rtt_us_symmetric_path_yields_zero_offset() {
        let (offset_us, rtt_us) = compute_offset_rtt_us(1_000_000, 1_010_000, 1_020_000, 1_030_000);
        assert_eq!(offset_us, 0, "symmetric path must yield zero offset");
        assert_eq!(
            rtt_us, 20_000,
            "total RTT (30ms) minus server dwell (10ms) = 20ms"
        );
    }

    /// #53 RED: a known asymmetric fixture (server clock ahead of ours) must
    /// match the textbook RFC 5905 formula exactly.
    #[test]
    fn compute_offset_rtt_us_matches_known_asymmetric_fixture() {
        let (offset_us, rtt_us) = compute_offset_rtt_us(1_000, 2_010, 2_020, 1_050);
        assert_eq!(
            offset_us, 990,
            "offset = ((2010-1000)+(2020-1050))/2 = (1010+970)/2 = 990"
        );
        assert_eq!(rtt_us, 40, "rtt = (1050-1000)-(2020-2010) = 50-10 = 40");
    }

    // ---- Ethernet/IPv4/UDP frame parsing ----

    /// Build a minimal Ethernet(14)+IPv4(20)+UDP(8)+payload frame.
    fn fake_udp_frame(src_ip: [u8; 4], dst_port: u16, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; 42 + payload.len()];
        frame[12] = 0x08; // EtherType IPv4
        frame[13] = 0x00;
        frame[23] = 17; // IP protocol = UDP
        frame[26..30].copy_from_slice(&src_ip);
        frame[36] = (dst_port >> 8) as u8;
        frame[37] = (dst_port & 0xFF) as u8;
        frame[42..].copy_from_slice(payload);
        frame
    }

    /// #53 RED: a well-formed IPv4/UDP frame must yield the real source IP,
    /// destination port, and payload — the naive stub always returns
    /// 0.0.0.0/port 0 regardless of the actual frame content.
    #[test]
    fn parse_udp_frame_extracts_source_ip_dst_port_and_payload() {
        let frame = fake_udp_frame([10, 77, 9, 204], 123, b"hello");
        let (src_ip, dst_port, payload) =
            parse_udp_frame(&frame).expect("well-formed frame must parse");
        assert_eq!(src_ip, Ipv4Addr::new(10, 77, 9, 204));
        assert_eq!(dst_port, 123);
        assert_eq!(payload, b"hello");
    }

    /// #53 RED: a non-IPv4 EtherType must be rejected, not silently accepted.
    #[test]
    fn parse_udp_frame_rejects_non_ipv4_ethertype() {
        let mut frame = fake_udp_frame([10, 0, 0, 1], 123, b"x");
        frame[12] = 0x86; // EtherType 0x86DD = IPv6
        frame[13] = 0xDD;
        assert!(
            parse_udp_frame(&frame).is_none(),
            "a non-IPv4 frame must be rejected"
        );
    }

    /// #53 RED: a non-UDP IP protocol must be rejected, not silently accepted.
    #[test]
    fn parse_udp_frame_rejects_non_udp_protocol() {
        let mut frame = fake_udp_frame([10, 0, 0, 1], 123, b"x");
        frame[23] = 6; // TCP
        assert!(
            parse_udp_frame(&frame).is_none(),
            "a non-UDP frame must be rejected"
        );
    }

    /// #53 RED: a frame shorter than the minimum Ethernet+IP+UDP header must
    /// be rejected.
    #[test]
    fn parse_udp_frame_rejects_too_short_frame() {
        let frame = [0u8; 20];
        assert!(
            parse_udp_frame(&frame).is_none(),
            "a 20-byte frame is too short to contain Ethernet+IP+UDP headers"
        );
    }
}
