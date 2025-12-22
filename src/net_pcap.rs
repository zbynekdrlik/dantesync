//! Npcap-based PTP network implementation for Windows.
//! Uses driver-level timestamps for precise packet timing.

use anyhow::{Result, anyhow};
use pcap::{Capture, Active, Device};
use std::net::{UdpSocket, Ipv4Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use log::{info, warn, debug};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

/// Create a socket and join PTP multicast group (for IGMP membership)
fn join_multicast(port: u16, iface_ip: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Socket, Domain, Type, Protocol};
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

/// PTP network using Npcap for precise packet timestamps
pub struct NpcapPtpNetwork {
    capture: Capture<Active>,
    // Keep sockets alive for IGMP multicast membership
    _igmp_sock_319: UdpSocket,
    _igmp_sock_320: UdpSocket,
}

impl NpcapPtpNetwork {
    pub fn new(interface_name: &str) -> Result<Self> {
        info!("Initializing Npcap capture on interface: {}", interface_name);

        // Find the device by name or description
        let devices = Device::list()?;
        let device = devices.iter()
            .find(|d| {
                d.name.contains(interface_name) ||
                d.desc.as_ref().map(|desc| desc.contains(interface_name)).unwrap_or(false)
            })
            .or_else(|| {
                // Try matching by IP address in description
                devices.iter().find(|d| {
                    d.addresses.iter().any(|addr| {
                        format!("{:?}", addr.addr).contains(interface_name)
                    })
                })
            })
            .ok_or_else(|| {
                let available: Vec<String> = devices.iter()
                    .map(|d| format!("{} ({:?})", d.name, d.desc))
                    .collect();
                anyhow!("Interface '{}' not found. Available: {:?}", interface_name, available)
            })?;

        info!("Found device: {} ({:?})", device.name, device.desc);

        // Extract interface IP for multicast join
        let iface_ip = device.addresses.iter()
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
        // This tells the switch to forward multicast traffic to us
        // Keep these sockets alive for the lifetime of the capture
        let igmp_sock_319 = join_multicast(PTP_EVENT_PORT, iface_ip)?;
        let igmp_sock_320 = join_multicast(PTP_GENERAL_PORT, iface_ip)?;
        info!("Joined PTP multicast group 224.0.1.129 on ports 319 and 320");

        // Open capture with immediate mode for lowest latency
        let capture = Capture::from_device(device.clone())?
            .promisc(true)         // Required to see multicast traffic
            .immediate_mode(true)  // Critical: disable buffering
            .snaplen(256)          // PTP packets are small
            .timeout(1)            // 1ms timeout for responsiveness
            .open()?;

        // Note: Npcap provides precise timestamps from the driver level
        info!("Npcap capture initialized (filtering by PTP ports in code)");

        Ok(NpcapPtpNetwork {
            capture,
            _igmp_sock_319: igmp_sock_319,
            _igmp_sock_320: igmp_sock_320,
        })
    }

}


impl crate::traits::PtpNetwork for NpcapPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        match self.capture.next_packet() {
            Ok(packet) => {
                let data = packet.data;

                // Use SystemTime::now() as receive timestamp instead of pcap timestamp
                // because pcap uses a monotonic clock that doesn't update when
                // we step the Windows system clock. The benefit of Npcap is the
                // low-latency driver-level capture, not the timestamp source.
                let ts = SystemTime::now();

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

                // Extract UDP payload
                let payload = &data[ETH_IP_UDP_HEADER..];
                let payload_len = payload.len();

                if payload_len > 0 {
                    let mut result = vec![0u8; payload_len];
                    result.copy_from_slice(payload);

                    debug!("[Npcap] PTP payload {} bytes, ts={:?}", payload_len, ts);
                    Ok(Some((result, payload_len, ts)))
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
    Ok(devices.iter()
        .map(|d| format!("{}: {:?}", d.name, d.desc))
        .collect())
}
