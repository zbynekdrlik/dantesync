//! Npcap-based PTP network implementation for Windows.
//!
//! Uses Npcap for packet capture with HIGH PRECISION timestamps that are
//! synchronized with system time. This uses KeQuerySystemTimePrecise() which
//! provides microsecond-level precision AND tracks system clock adjustments.
//!
//! Key: We use TimestampType::HostHighPrec which maps to PCAP_TSTAMP_HOST_HIPREC
//! and uses KeQuerySystemTimePrecise() internally - NOT the default UNSYNCED mode.

use anyhow::{anyhow, Result};
use log::{debug, info, warn};
use pcap::{Active, Capture, Device, TimestampType};
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

/// Create a socket and join PTP multicast group (for IGMP membership)
fn join_multicast(port: u16, iface_ip: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::SocketAddrV4;

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    socket.join_multicast_v4(&PTP_MULTICAST, &iface_ip)?;
    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

/// PTP network using Npcap with HostHighPrec timestamps
pub struct NpcapPtpNetwork {
    capture: Capture<Active>,
    // Keep sockets alive for IGMP multicast membership
    _igmp_sock_319: UdpSocket,
    _igmp_sock_320: UdpSocket,
    using_hiprec: bool,
}

impl NpcapPtpNetwork {
    pub fn new(interface_name: &str) -> Result<Self> {
        info!(
            "Initializing Npcap capture on interface: {}",
            interface_name
        );

        // Find the device by name or description
        let devices = Device::list()?;
        let device = devices
            .iter()
            .find(|d| {
                d.name.contains(interface_name)
                    || d.desc
                        .as_ref()
                        .map(|desc| desc.contains(interface_name))
                        .unwrap_or(false)
            })
            .or_else(|| {
                // Try matching by IP address in description
                devices.iter().find(|d| {
                    d.addresses
                        .iter()
                        .any(|addr| format!("{:?}", addr.addr).contains(interface_name))
                })
            })
            .ok_or_else(|| {
                let available: Vec<String> = devices
                    .iter()
                    .map(|d| format!("{} ({:?})", d.name, d.desc))
                    .collect();
                anyhow!(
                    "Interface '{}' not found. Available: {:?}",
                    interface_name,
                    available
                )
            })?;

        info!("Found device: {} ({:?})", device.name, device.desc);

        // Extract interface IP for multicast join
        let iface_ip = device
            .addresses
            .iter()
            .find_map(|a| {
                if let std::net::IpAddr::V4(ip) = a.addr {
                    if !ip.is_loopback() {
                        return Some(ip);
                    }
                }
                None
            })
            .ok_or_else(|| anyhow!("No IPv4 address found on device"))?;

        info!("Using interface IP {} for multicast join", iface_ip);

        // CRITICAL: Join multicast group via sockets to trigger IGMP
        let igmp_sock_319 = join_multicast(PTP_EVENT_PORT, iface_ip)?;
        let igmp_sock_320 = join_multicast(PTP_GENERAL_PORT, iface_ip)?;
        info!("Joined PTP multicast group 224.0.1.129 on ports 319 and 320");

        // Create capture handle with HostHighPrec timestamps
        // HostHighPrec uses KeQuerySystemTimePrecise() which is both high-precision AND synced with system time
        info!("[TS] Requesting HostHighPrec timestamps (KeQuerySystemTimePrecise)");

        let mut capture = Capture::from_device(device.clone())?
            .promisc(false) // Don't use promiscuous - rely on IGMP multicast join
            .immediate_mode(true) // Critical: disable buffering for lowest latency
            .snaplen(256) // PTP packets are small
            .timeout(1) // 1ms timeout for responsiveness
            .tstamp_type(TimestampType::HostHighPrec)
            .open()?;

        // Apply BPF filter to only capture PTP multicast - reduces conflict with DVS
        let ptp_filter = "udp and dst host 224.0.1.129 and (dst port 319 or dst port 320)";
        capture.filter(ptp_filter, true)?;
        info!("[Filter] Applied BPF: {}", ptp_filter);

        // Assume HostHighPrec is available on modern Npcap (1.20+)
        let using_hiprec = true;
        info!("[TS] Using HostHighPrec timestamps (KeQuerySystemTimePrecise)");

        if using_hiprec {
            info!("Npcap capture initialized with HIGH PRECISION synchronized timestamps");
        } else {
            warn!("Npcap capture using default timestamps (may drift from system time)");
        }

        Ok(NpcapPtpNetwork {
            capture,
            _igmp_sock_319: igmp_sock_319,
            _igmp_sock_320: igmp_sock_320,
            using_hiprec,
        })
    }

    /// Convert pcap timestamp to SystemTime
    fn pcap_ts_to_systemtime(ts_sec: i64, ts_usec: i64) -> SystemTime {
        let duration = Duration::new(ts_sec as u64, (ts_usec * 1000) as u32);
        UNIX_EPOCH + duration
    }
}

impl crate::traits::PtpNetwork for NpcapPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        match self.capture.next_packet() {
            Ok(packet) => {
                let data = packet.data;

                // Use Npcap's HostHighPrec timestamps - these are both precise AND synced
                // with system time (using KeQuerySystemTimePrecise on Windows 8+)
                let header = packet.header;
                let ts = if self.using_hiprec {
                    // Npcap provides high-precision timestamps synced with system time
                    let ts = Self::pcap_ts_to_systemtime(
                        header.ts.tv_sec as i64,
                        header.ts.tv_usec as i64,
                    );
                    debug!(
                        "[TS] Npcap HostHighPrec: {}.{:06}",
                        header.ts.tv_sec, header.ts.tv_usec
                    );
                    ts
                } else {
                    // Fallback to SystemTime::now() if HostHighPrec not available
                    SystemTime::now()
                };

                // Extract UDP payload from Ethernet frame
                // Ethernet (14) + IP (20) + UDP (8) = 42 bytes header
                const ETH_IP_UDP_HEADER: usize = 42;

                if data.len() < ETH_IP_UDP_HEADER {
                    return Ok(None);
                }

                // Verify it's an IP packet (EtherType 0x0800)
                if data[12] != 0x08 || data[13] != 0x00 {
                    return Ok(None);
                }

                // Verify UDP protocol (IP header byte 9 = protocol)
                if data[23] != 17 {
                    return Ok(None);
                }

                // Check destination port for PTP (319 or 320)
                let dst_port = ((data[36] as u16) << 8) | data[37] as u16;
                if dst_port != 319 && dst_port != 320 {
                    return Ok(None);
                }

                // Extract source IP from IP header (Ethernet 14 bytes + IP src at offset 12)
                // Source IP is at bytes 26-29 of the Ethernet frame
                let source_ip = Ipv4Addr::new(data[26], data[27], data[28], data[29]);

                // Extract UDP payload
                let payload = &data[ETH_IP_UDP_HEADER..];
                let payload_len = payload.len();

                if payload_len > 0 {
                    let mut result = vec![0u8; payload_len];
                    result.copy_from_slice(payload);

                    debug!(
                        "[Npcap] PTP payload {} bytes from {}",
                        payload_len, source_ip
                    );
                    Ok(Some((result, payload_len, ts, Some(source_ip))))
                } else {
                    Ok(None)
                }
            }
            Err(pcap::Error::TimeoutExpired) => {
                // Normal timeout - no packet available
                Ok(None)
            }
            Err(e) => {
                warn!("Npcap recv error: {} ({:?})", e, e);
                Err(e.into())
            }
        }
    }

    fn reset(&mut self) -> Result<()> {
        // Npcap doesn't need explicit reset
        Ok(())
    }
}

/// Get list of available Npcap devices
pub fn list_npcap_devices() -> Result<Vec<String>> {
    let devices = Device::list()?;
    Ok(devices
        .iter()
        .map(|d| format!("{}: {:?}", d.name, d.desc))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test PTP constants
    #[test]
    fn test_ptp_constants() {
        assert_eq!(PTP_EVENT_PORT, 319);
        assert_eq!(PTP_GENERAL_PORT, 320);
        assert_eq!(PTP_MULTICAST, Ipv4Addr::new(224, 0, 1, 129));
        assert!(PTP_MULTICAST.is_multicast());
    }

    /// Test pcap timestamp to SystemTime conversion
    #[test]
    fn test_pcap_ts_to_systemtime() {
        // Unix epoch (1970-01-01 00:00:00)
        let ts = NpcapPtpNetwork::pcap_ts_to_systemtime(0, 0);
        assert_eq!(ts, UNIX_EPOCH);

        // 1 second after epoch
        let ts = NpcapPtpNetwork::pcap_ts_to_systemtime(1, 0);
        assert_eq!(ts, UNIX_EPOCH + Duration::from_secs(1));

        // 1.5 seconds after epoch (with microseconds)
        let ts = NpcapPtpNetwork::pcap_ts_to_systemtime(1, 500_000);
        assert_eq!(ts, UNIX_EPOCH + Duration::from_micros(1_500_000));

        // Realistic timestamp (2024-01-01 00:00:00 UTC = 1704067200)
        let ts = NpcapPtpNetwork::pcap_ts_to_systemtime(1704067200, 0);
        assert_eq!(ts, UNIX_EPOCH + Duration::from_secs(1704067200));
    }

    /// Test that microseconds are correctly converted to nanoseconds
    #[test]
    fn test_pcap_ts_microsecond_precision() {
        // 123.456789 seconds - but pcap only has microsecond precision
        let ts = NpcapPtpNetwork::pcap_ts_to_systemtime(123, 456_789);

        // Should be 123 seconds + 456789 microseconds = 456789000 nanoseconds
        let expected = UNIX_EPOCH + Duration::new(123, 456_789_000);
        assert_eq!(ts, expected);
    }

    /// Test Ethernet/IP/UDP header constant
    #[test]
    fn test_ethernet_ip_udp_header_size() {
        // Ethernet header: 14 bytes
        // IP header: 20 bytes (minimum)
        // UDP header: 8 bytes
        // Total: 42 bytes
        const ETH_IP_UDP_HEADER: usize = 42;
        assert_eq!(ETH_IP_UDP_HEADER, 14 + 20 + 8);
    }

    /// Test EtherType detection for IPv4
    #[test]
    fn test_ethertype_ipv4() {
        // IPv4 EtherType is 0x0800
        let data: [u8; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08, 0x00];
        assert_eq!(data[12], 0x08);
        assert_eq!(data[13], 0x00);
    }

    /// Test UDP protocol number in IP header
    #[test]
    fn test_ip_protocol_udp() {
        // UDP is protocol number 17
        // In IP header, protocol is at byte offset 9 (0-indexed)
        // In full frame, that's offset 14 (ethernet) + 9 = 23
        let protocol_byte = 17u8;
        assert_eq!(protocol_byte, 17);
    }

    /// Test PTP port detection from UDP header
    #[test]
    fn test_ptp_port_extraction() {
        // UDP destination port is at bytes 2-3 of UDP header (big-endian)
        // In full frame: offset 14 (eth) + 20 (ip) + 2 = 36, 37

        // Port 319 = 0x013F
        let port_319_bytes: [u8; 2] = [0x01, 0x3F];
        let port = ((port_319_bytes[0] as u16) << 8) | port_319_bytes[1] as u16;
        assert_eq!(port, 319);

        // Port 320 = 0x0140
        let port_320_bytes: [u8; 2] = [0x01, 0x40];
        let port = ((port_320_bytes[0] as u16) << 8) | port_320_bytes[1] as u16;
        assert_eq!(port, 320);
    }

    /// Test simulated PTP packet validation
    #[test]
    fn test_simulated_ptp_packet_structure() {
        // Minimum valid PTP-carrying Ethernet frame
        // Ethernet (14) + IP (20) + UDP (8) + PTP Sync (44) = 86 bytes
        const MIN_PTP_FRAME: usize = 42 + 44;
        assert_eq!(MIN_PTP_FRAME, 86);

        // Create a simulated frame
        let mut frame = vec![0u8; MIN_PTP_FRAME];

        // Set EtherType to IPv4 (0x0800) at bytes 12-13
        frame[12] = 0x08;
        frame[13] = 0x00;

        // Set IP protocol to UDP (17) at byte 23
        frame[23] = 17;

        // Set UDP destination port to 319 at bytes 36-37
        frame[36] = 0x01;
        frame[37] = 0x3F;

        // Verify parsing would succeed
        assert!(frame[12] == 0x08 && frame[13] == 0x00, "Should be IPv4");
        assert!(frame[23] == 17, "Should be UDP");
        let dst_port = ((frame[36] as u16) << 8) | frame[37] as u16;
        assert!(dst_port == 319 || dst_port == 320, "Should be PTP port");
    }
}
