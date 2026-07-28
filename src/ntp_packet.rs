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
/// #53 GREEN: seconds shift by the epoch delta; the 32-bit binary fraction
/// converts to microseconds via `(fraction * 1_000_000) >> 32` (done in u64
/// to avoid overflowing u32's ~4.29e9 range against the ~4.3e15 intermediate
/// product).
pub fn ntp_timestamp_to_unix_micros(seconds: u32, fraction: u32) -> i64 {
    let unix_secs = seconds as i64 - NTP_UNIX_EPOCH_DELTA_SECS;
    let frac_us = (fraction as u64 * 1_000_000) >> 32;
    unix_secs * 1_000_000 + frac_us as i64
}

/// Convert microseconds-since-Unix-epoch back into an NTP (seconds, fraction)
/// pair. Handles pre-1970 values (negative `unix_us`) via Euclidean division
/// so the fractional part always stays non-negative.
///
/// #53 GREEN: the inverse of `ntp_timestamp_to_unix_micros` — the
/// microsecond remainder scales up to the 32-bit binary fraction via
/// `(rem_us << 32) / 1_000_000` (done in u64 for the same overflow reason).
pub fn unix_micros_to_ntp_timestamp(unix_us: i64) -> (u32, u32) {
    let unix_secs = unix_us.div_euclid(1_000_000);
    let rem_us = unix_us.rem_euclid(1_000_000);
    let seconds = (unix_secs + NTP_UNIX_EPOCH_DELTA_SECS) as u32;
    let fraction = ((rem_us as u64) << 32) / 1_000_000;
    (seconds, fraction as u32)
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
/// #53 GREEN: sets LI=0/VN=4/Mode=3 in byte 0 and encodes `transmit_ts_us`
/// into the Transmit Timestamp field (bytes 40..48).
pub fn build_client_request(transmit_ts_us: i64) -> [u8; NTP_PACKET_LEN] {
    let mut packet = [0u8; NTP_PACKET_LEN];
    packet[0] = (NTP_VERSION << 3) | NTP_MODE_CLIENT;
    let (secs, frac) = unix_micros_to_ntp_timestamp(transmit_ts_us);
    packet[40..44].copy_from_slice(&secs.to_be_bytes());
    packet[44..48].copy_from_slice(&frac.to_be_bytes());
    packet
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
/// #53 GREEN: rejects a too-short buffer and a non-server mode, then decodes
/// the real Origin/Receive/Transmit Timestamp fields.
pub fn parse_reply(buf: &[u8]) -> Result<ParsedReply> {
    if buf.len() < NTP_PACKET_LEN {
        return Err(anyhow!(
            "NTP reply too short: {} bytes (need {})",
            buf.len(),
            NTP_PACKET_LEN
        ));
    }
    let mode = buf[0] & NTP_MODE_MASK;
    if mode != NTP_MODE_SERVER {
        return Err(anyhow!(
            "NTP reply has unexpected mode {} (expected server={})",
            mode,
            NTP_MODE_SERVER
        ));
    }
    let read_ts = |off: usize| -> i64 {
        let secs = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap());
        let frac = u32::from_be_bytes(buf[off + 4..off + 8].try_into().unwrap());
        ntp_timestamp_to_unix_micros(secs, frac)
    };
    Ok(ParsedReply {
        origin_ts_us: read_ts(24),
        receive_ts_us: read_ts(32),
        transmit_ts_us: read_ts(40),
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
/// #53 GREEN: the real formulas.
pub fn compute_offset_rtt_us(t1_us: i64, t2_us: i64, t3_us: i64, t4_us: i64) -> (i64, i64) {
    let offset_us = ((t2_us - t1_us) + (t3_us - t4_us)) / 2;
    let rtt_us = (t4_us - t1_us) - (t3_us - t2_us);
    (offset_us, rtt_us)
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
/// #53 GREEN: validates length, EtherType (IPv4 = 0x0800), and IP protocol
/// (UDP = 17) before extracting the real source IP, destination port, and
/// payload.
pub fn parse_udp_frame(data: &[u8]) -> Option<(Ipv4Addr, u16, &[u8])> {
    const ETH_IP_UDP_HEADER: usize = 42;
    if data.len() < ETH_IP_UDP_HEADER {
        return None;
    }
    if data[12] != 0x08 || data[13] != 0x00 {
        return None; // not IPv4
    }
    if data[23] != 17 {
        return None; // not UDP
    }
    let src_ip = Ipv4Addr::new(data[26], data[27], data[28], data[29]);
    let dst_port = ((data[36] as u16) << 8) | data[37] as u16;
    Some((src_ip, dst_port, &data[ETH_IP_UDP_HEADER..]))
}

// ============================================================================
// #53 continuation — select the pcap NTP device by NTP-server reachability
// ============================================================================
// v1.8.24 confirmed live on a dual-homed host (stream box: Dante 10.77.7.204/23,
// LAN 10.77.9.204/23) that `PcapNtpTransport` inheriting the PTP capture
// interface can never work when PTP and NTP live on different subnets — the
// NTP server is genuinely unreachable from the PTP-selected device
// (WSAENETUNREACH / os error 10051), so the transport silently degrades to
// userspace rsntp for every sample, forever. The fix: pick the pcap device by
// which interface's subnet actually CONTAINS the (resolved) NTP server
// address — the same decision `Find-NetRoute` makes, made automatically and
// testably. Pure, zero-pcap-dependency, so it is unit-tested on Linux CI
// exactly like the wire-format math above.
// ============================================================================

/// One Npcap-visible network interface candidate for NTP-transport device
/// selection: a device's name plus ONE of its IPv4 address/netmask pairs,
/// pulled out of the platform-specific `pcap::Device` into a plain value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateInterface {
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Option<Ipv4Addr>,
}

/// Whether `target` falls inside the subnet defined by `ip`/`netmask`
/// (standard IP containment: masking both addresses must produce the same
/// network prefix).
fn same_subnet(ip: Ipv4Addr, netmask: Ipv4Addr, target: Ipv4Addr) -> bool {
    (u32::from(ip) & u32::from(netmask)) == (u32::from(target) & u32::from(netmask))
}

/// Select which `candidates` entry can actually reach `server_ip` — never by
/// name, only by whether the candidate's OWN subnet contains the server
/// address. On more than one match, the MOST SPECIFIC (longest-prefix /
/// smallest) subnet wins, mirroring standard IP routing "longest prefix
/// match" — a stable sort keeps the first-listed candidate on an exact tie.
/// Returns `None` when no candidate's subnet contains `server_ip`.
///
/// #53 GREEN: real subnet-containment selection instead of a name-based or
/// arrival-order guess.
pub fn select_ntp_pcap_device(
    server_ip: Ipv4Addr,
    candidates: &[CandidateInterface],
) -> Option<usize> {
    let mut matches: Vec<(usize, u32)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let netmask = c.netmask?;
            if same_subnet(c.ip, netmask, server_ip) {
                Some((i, u32::from(netmask).count_ones()))
            } else {
                None
            }
        })
        .collect();
    // Stable sort descending by prefix length -- an exact tie keeps the
    // original (first-listed) relative order.
    matches.sort_by_key(|&(_, prefix_len)| std::cmp::Reverse(prefix_len));
    matches.first().map(|&(i, _)| i)
}

/// Whether the kernel-timestamped Npcap NTP transport was used for EVERY
/// sample in a burst — `false` the instant even one sample fell back to
/// userspace rsntp, and `false` for an empty burst.
///
/// #53 GREEN: a real "every flag true, and non-empty" check.
pub fn burst_used_pcap_throughout(via_pcap_flags: &[bool]) -> bool {
    !via_pcap_flags.is_empty() && via_pcap_flags.iter().all(|&v| v)
}

// ============================================================================
// #53 continuation (adversarial-review fix) — reject stale/mis-paired
// captured NTP replies by Origin Timestamp correlation
// ============================================================================
// `PcapNtpTransport::measure_once` (net_pcap.rs) used to accept the FIRST
// captured packet from `server_ip` parsing as a mode-4 reply as t4, with NO
// check that it actually answers the request THIS call just sent. Two real
// sources of a stale/foreign reply landing in the same 500ms capture window:
// (1) a reply left over in the capture queue from a PRIOR `measure_once`
// call (mitigated by draining the queue before sending, in net_pcap.rs), and
// (2) the userspace `rsntp` fallback path sending its own independent
// request to the same `server_ip:123` — which the open BPF filter
// (`udp and host <server> and port 123`) also matches — so its reply can be
// captured and mistaken for ours even with the queue freshly drained.
// `parse_reply` already decodes the reply's Origin Timestamp (the server's
// verbatim echo of OUR request's Transmit Timestamp field) but nothing ever
// checked it against the request we actually just sent. Fix: compare the
// reply's `origin_ts_us` to the `request_transmit_ts_us` embedded when we
// built the request; a REAL reply to THIS request matches within the ~1us
// NTP-fixed-point encode/decode rounding already proven exact by
// `unix_micros_ntp_roundtrip_is_within_one_microsecond_for_arbitrary_values`
// above — anything else is a different exchange (a check runs every
// 10-30s, so a stale reply differs by many orders of magnitude more than
// 1us) and must be rejected, not paired.
// ============================================================================

/// Whether a captured reply's echoed Origin Timestamp matches the request we
/// actually just sent — the correlation check that used to be missing
/// entirely from `PcapNtpTransport::measure_once`. A tolerance of 1us covers
/// the NTP 32-bit fixed-point encode/decode rounding (see the round-trip
/// tests above); a genuinely stale/foreign reply (a different measurement
/// interval, or the rsntp fallback's own independent exchange to the same
/// server) differs by milliseconds-to-seconds, far outside this tolerance.
///
/// #53 GREEN: a real bounded-difference check.
pub fn reply_origin_matches_request(origin_ts_us: i64, request_transmit_ts_us: i64) -> bool {
    (origin_ts_us - request_transmit_ts_us).abs() <= 1
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- NTP <-> Unix epoch conversion ----

    /// #53 GREEN: converting the NTP epoch itself (seconds = the delta,
    /// fraction = 0) must land exactly on unix_us = 0.
    #[test]
    fn ntp_timestamp_to_unix_micros_at_ntp_epoch_boundary_is_zero() {
        let unix_us = ntp_timestamp_to_unix_micros(NTP_UNIX_EPOCH_DELTA_SECS as u32, 0);
        assert_eq!(
            unix_us, 0,
            "NTP epoch delta with zero fraction is unix_us=0"
        );
    }

    /// #53 GREEN: a half-second fraction (2^31 / 2^32 = 0.5) must decode to
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

    /// #53 GREEN: encoding then decoding the NTP epoch (unix_us=0) and an exact
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

    /// #53 GREEN: an arbitrary microsecond value must round-trip within 1us
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

    /// #53 GREEN: byte 0 must be LI=0/VN=4/Mode=3 = 0x23. The naive stub
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

    /// #53 GREEN: the Transmit Timestamp field (bytes 40..48) must decode back
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

    /// #53 GREEN: a well-formed server reply must yield the exact origin/
    /// receive/transmit values that were encoded into it.
    #[test]
    fn parse_reply_extracts_origin_receive_and_transmit_timestamps() {
        // Half-second-aligned values so the 32-bit NTP fraction encode/decode
        // round-trips EXACTLY (see unix_micros_ntp_roundtrip_is_exact_at_whole_and_half_second_boundaries)
        // — an arbitrary microsecond value can lose up to 1us to floor
        // rounding in this encoding, which is a property of the wire format
        // being tested separately above, not something this test needs to
        // re-litigate.
        let buf = fake_server_reply(1_000_000, 1_500_000, 2_000_000);
        let reply = parse_reply(&buf).expect("well-formed server reply must parse");
        assert_eq!(reply.origin_ts_us, 1_000_000);
        assert_eq!(reply.receive_ts_us, 1_500_000);
        assert_eq!(reply.transmit_ts_us, 2_000_000);
    }

    /// #53 GREEN: a buffer shorter than the 48-byte NTP header must be
    /// rejected, not silently accepted with garbage/zero fields.
    #[test]
    fn parse_reply_rejects_too_short_buffer() {
        let buf = [0u8; 10];
        assert!(
            parse_reply(&buf).is_err(),
            "a 10-byte buffer is not a valid NTP packet and must be rejected"
        );
    }

    /// #53 GREEN: a packet with the CLIENT mode (3) — e.g. our own request
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

    /// #53 GREEN: a perfectly symmetric network path (equal delay each way)
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

    /// #53 GREEN: a known asymmetric fixture (server clock ahead of ours) must
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

    /// #53 GREEN: a well-formed IPv4/UDP frame must yield the real source IP,
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

    /// #53 GREEN: a non-IPv4 EtherType must be rejected, not silently accepted.
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

    /// #53 GREEN: a non-UDP IP protocol must be rejected, not silently accepted.
    #[test]
    fn parse_udp_frame_rejects_non_udp_protocol() {
        let mut frame = fake_udp_frame([10, 0, 0, 1], 123, b"x");
        frame[23] = 6; // TCP
        assert!(
            parse_udp_frame(&frame).is_none(),
            "a non-UDP frame must be rejected"
        );
    }

    /// #53 GREEN: a frame shorter than the minimum Ethernet+IP+UDP header must
    /// be rejected.
    #[test]
    fn parse_udp_frame_rejects_too_short_frame() {
        let frame = [0u8; 20];
        assert!(
            parse_udp_frame(&frame).is_none(),
            "a 20-byte frame is too short to contain Ethernet+IP+UDP headers"
        );
    }

    // ---- #53 continuation: pcap NTP device selection by reachability ----

    /// #53 RED: reproduces the EXACT live stream-box topology — Dante on
    /// 10.77.7.204/23 (where the PTP capture binds), LAN on 10.77.9.204/23
    /// (where the NTP server actually lives). The LAN candidate must be
    /// selected because ONLY its subnet contains the server address; the
    /// placeholder just returns the first candidate with a netmask (Dante,
    /// listed first) regardless of `server_ip`, so this fails against it.
    #[test]
    fn select_ntp_pcap_device_picks_the_subnet_that_contains_the_server() {
        let server_ip: Ipv4Addr = "10.77.9.202".parse().unwrap();
        let candidates = [
            CandidateInterface {
                name: "Ethernet 2 (Dante)".to_string(),
                ip: "10.77.7.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23
            },
            CandidateInterface {
                name: "Ethernet (LAN)".to_string(),
                ip: "10.77.9.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23
            },
        ];

        let selected = select_ntp_pcap_device(server_ip, &candidates);

        assert_eq!(
            selected,
            Some(1),
            "the LAN device (index 1) is the only one whose /23 subnet contains \
             10.77.9.202 -- Dante's 10.77.6.0-10.77.7.255 does not overlap LAN's \
             10.77.8.0-10.77.9.255"
        );
    }

    /// #53 RED: when NO candidate's subnet contains the server address, the
    /// selector must return `None` rather than guessing a device.
    #[test]
    fn select_ntp_pcap_device_returns_none_when_no_candidate_can_reach_server() {
        let server_ip: Ipv4Addr = "192.168.1.5".parse().unwrap();
        let candidates = [
            CandidateInterface {
                name: "Ethernet 2 (Dante)".to_string(),
                ip: "10.77.7.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()),
            },
            CandidateInterface {
                name: "Ethernet (LAN)".to_string(),
                ip: "10.77.9.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()),
            },
        ];

        assert_eq!(
            select_ntp_pcap_device(server_ip, &candidates),
            None,
            "no candidate's /23 subnet contains 192.168.1.5 -- must be None, not a guess"
        );
    }

    /// #53 RED: on overlap (more than one candidate's subnet contains the
    /// server), the MOST SPECIFIC (longest-prefix / smallest) subnet must
    /// win -- mirrors standard IP routing "longest prefix match". The
    /// placeholder ignores this entirely (just picks the first with a
    /// netmask), so this fails when the broader (/16) candidate is listed
    /// first.
    #[test]
    fn select_ntp_pcap_device_prefers_longest_prefix_on_overlap() {
        let server_ip: Ipv4Addr = "10.77.9.202".parse().unwrap();
        let candidates = [
            CandidateInterface {
                name: "broad /16".to_string(),
                ip: "10.77.0.1".parse().unwrap(),
                netmask: Some("255.255.0.0".parse().unwrap()), // /16, also contains the server
            },
            CandidateInterface {
                name: "specific /23".to_string(),
                ip: "10.77.9.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23, more specific
            },
        ];

        assert_eq!(
            select_ntp_pcap_device(server_ip, &candidates),
            Some(1),
            "both subnets contain the server, but the /23 (index 1) is more specific \
             than the /16 (index 0) and must win"
        );
    }

    /// #53 RED: a candidate with no netmask (e.g. an interface pcap couldn't
    /// fully enumerate) can never be selected, even if its bare address
    /// happens to share the server's first octets.
    #[test]
    fn select_ntp_pcap_device_skips_candidates_without_netmask() {
        let server_ip: Ipv4Addr = "10.77.9.202".parse().unwrap();
        let candidates = [CandidateInterface {
            name: "no netmask".to_string(),
            ip: "10.77.9.1".parse().unwrap(),
            netmask: None,
        }];

        assert_eq!(
            select_ntp_pcap_device(server_ip, &candidates),
            None,
            "a candidate with no netmask must never be selected"
        );
    }

    /// Adversarial-review fix (#53 continuation): the FOUR tests above never
    /// actually exercise the /23 vs /24 distinction the implementation
    /// claims to make -- in every one of them, the winning candidate ALSO
    /// shares its first three octets with the server (a naive "compare
    /// octet 1-2-3" /24 implementation would coincidentally pass all four).
    /// This fixture forces the real subnet-mask math: the server
    /// (10.77.6.5) is inside the Dante candidate's /23 span
    /// (10.77.6.0-10.77.7.255, from IP 10.77.7.204/23) but in a DIFFERENT
    /// /24 than that candidate's own IP (10.77.6.x vs 10.77.7.x) -- a naive
    /// octet-1-2-3-equality implementation returns None here (matches
    /// neither candidate), while the real netmask-AND implementation must
    /// select the Dante candidate (index 0).
    #[test]
    fn select_ntp_pcap_device_selects_across_a_24_boundary_within_the_23_span() {
        let server_ip: Ipv4Addr = "10.77.6.5".parse().unwrap();
        let candidates = [
            CandidateInterface {
                name: "Ethernet 2 (Dante)".to_string(),
                ip: "10.77.7.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23: 10.77.6.0-10.77.7.255
            },
            CandidateInterface {
                name: "Ethernet (LAN)".to_string(),
                ip: "10.77.9.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23: 10.77.8.0-10.77.9.255
            },
        ];

        assert_eq!(
            select_ntp_pcap_device(server_ip, &candidates),
            Some(0),
            "10.77.6.5 is inside the Dante candidate's /23 span (10.77.6.0-10.77.7.255) even \
             though it's in a DIFFERENT /24 (10.77.6.x) than the candidate's own IP (10.77.7.x) \
             -- a /24-only implementation would wrongly return None here"
        );
    }

    /// Same class as above, mirrored onto the LAN candidate: the server
    /// (10.77.8.50) is inside the LAN candidate's /23 span
    /// (10.77.8.0-10.77.9.255, from IP 10.77.9.204/23) but in a DIFFERENT
    /// /24 (10.77.8.x vs the candidate's own 10.77.9.x). A naive /24
    /// implementation returns None here too; the real implementation must
    /// select the LAN candidate (index 1).
    #[test]
    fn select_ntp_pcap_device_selects_across_a_24_boundary_within_the_23_span_other_candidate() {
        let server_ip: Ipv4Addr = "10.77.8.50".parse().unwrap();
        let candidates = [
            CandidateInterface {
                name: "Ethernet 2 (Dante)".to_string(),
                ip: "10.77.7.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23: 10.77.6.0-10.77.7.255
            },
            CandidateInterface {
                name: "Ethernet (LAN)".to_string(),
                ip: "10.77.9.204".parse().unwrap(),
                netmask: Some("255.255.254.0".parse().unwrap()), // /23: 10.77.8.0-10.77.9.255
            },
        ];

        assert_eq!(
            select_ntp_pcap_device(server_ip, &candidates),
            Some(1),
            "10.77.8.50 is inside the LAN candidate's /23 span (10.77.8.0-10.77.9.255) even \
             though it's in a DIFFERENT /24 (10.77.8.x) than the candidate's own IP (10.77.9.x) \
             -- a /24-only implementation would wrongly return None here"
        );
    }

    /// #53 RED: the aggregate must be `true` only when EVERY sample in the
    /// burst used the pcap transport. The placeholder only checks the FIRST
    /// flag, so a burst that starts pcap=true but degrades partway through
    /// (a transient per-sample fallback) would be wrongly reported active.
    #[test]
    fn burst_used_pcap_throughout_true_only_when_all_true() {
        assert!(burst_used_pcap_throughout(&[true, true, true]));
        assert!(
            !burst_used_pcap_throughout(&[true, true, false]),
            "one fallback sample must flip the whole burst to NOT active"
        );
        assert!(
            !burst_used_pcap_throughout(&[false, true, true]),
            "a leading fallback sample must also flip it -- not just a trailing one"
        );
    }

    /// #53 RED: an empty burst (every sample failed outright) must never be
    /// reported as "pcap active" — no samples is not "active".
    #[test]
    fn burst_used_pcap_throughout_false_for_empty_slice() {
        assert!(!burst_used_pcap_throughout(&[]));
    }

    // ---- #53 continuation (adversarial-review fix): reply-origin correlation ----

    /// #53 RED: an exact echo of the request's transmit timestamp must match.
    #[test]
    fn reply_origin_matches_request_exact_match_is_true() {
        assert!(reply_origin_matches_request(1_000_000, 1_000_000));
    }

    /// #53 RED: a 1us difference is within the NTP fixed-point encode/decode
    /// rounding tolerance and must still match.
    #[test]
    fn reply_origin_matches_request_within_one_microsecond_rounding_is_true() {
        assert!(reply_origin_matches_request(1_000_001, 1_000_000));
        assert!(reply_origin_matches_request(999_999, 1_000_000));
    }

    /// #53 RED: a 2us difference is already outside the encode/decode
    /// rounding tolerance and must be rejected.
    #[test]
    fn reply_origin_matches_request_two_microseconds_off_is_false() {
        assert!(!reply_origin_matches_request(1_000_002, 1_000_000));
    }

    /// #53 RED: this is the actual defect fixture — a reply whose origin
    /// timestamp is from a MUCH earlier request (e.g. the previous
    /// `measure_once` call's request, one 10-30s check-interval ago, or the
    /// rsntp fallback's own independent exchange to the same server) must be
    /// rejected as a stale/foreign reply, never paired with the current
    /// request. The naive "accept whatever comes from server_ip" behavior
    /// this replaces would have paired them.
    #[test]
    fn reply_origin_matches_request_rejects_a_reply_from_a_stale_prior_interval() {
        let request_transmit_ts_us = 50_000_000_000i64; // "now" for this call
        let stale_origin_ts_us = request_transmit_ts_us - 15_000_000; // 15s earlier
        assert!(
            !reply_origin_matches_request(stale_origin_ts_us, request_transmit_ts_us),
            "a reply echoing a 15s-old origin timestamp must never be accepted as this \
             request's reply"
        );
    }
}
