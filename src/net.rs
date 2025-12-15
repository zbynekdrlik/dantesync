use anyhow::{Result, anyhow};
use pnet_datalink::{self, NetworkInterface};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};

pub fn get_default_interface() -> Result<(NetworkInterface, Ipv4Addr)> {
    let interfaces = pnet_datalink::interfaces();
    
    // Strategy 1: Find interface with default gateway (not easily possible with pnet without routing table, 
    // but we can look for Up, Running, !Loopback, and prefer Ethernet)
    
    // Filter for usable interfaces (IPv4, Up, not Loopback)
    let usable_interfaces: Vec<&NetworkInterface> = interfaces.iter()
        .filter(|iface| iface.is_up() && !iface.is_loopback() && !iface.ips.is_empty())
        .collect();

    if usable_interfaces.is_empty() {
        return Err(anyhow!("No suitable network interface found"));
    }

    // Try to find a wired connection first (heuristics based on name or flags if available)
    let mut best_iface = None;
    let mut best_ip = None;

    for iface in usable_interfaces {
        // Find IPv4
        let ipv4 = iface.ips.iter().find(|ip| ip.is_ipv4()).map(|ip| {
             if let IpAddr::V4(addr) = ip.ip() { addr } else { unreachable!() }
        });

        if let Some(ip) = ipv4 {
            let name_lower = iface.name.to_lowercase();
            let is_likely_wireless = name_lower.contains("wlan") || name_lower.contains("wifi") || name_lower.contains("wireless");
            
            if !is_likely_wireless {
                best_iface = Some(iface.clone());
                best_ip = Some(ip);
                break; // Found a likely wired one
            } else if best_iface.is_none() {
                // Fallback to wireless if nothing else
                best_iface = Some(iface.clone());
                best_ip = Some(ip);
            }
        }
    }

    match (best_iface, best_ip) {
        (Some(iface), Some(ip)) => Ok((iface, ip)),
        _ => Err(anyhow!("No suitable IPv4 interface found")),
    }
}

pub fn create_multicast_socket(port: u16, interface_ip: Ipv4Addr) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    
    // Reuse address/port
    socket.set_reuse_address(true)?;
    // set_reuse_port is not consistently available via socket2 Socket struct across platforms/versions without traits.
    // reuse_address is usually sufficient for multicast.
    
    // Bind to ANY address on the specific port
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    // Join multicast group
    let multi_addr: Ipv4Addr = "224.0.1.129".parse()?;
    socket.join_multicast_v4(&multi_addr, &interface_ip)?;
    
    // Disable multicast loopback
    socket.set_multicast_loop_v4(false)?;

    socket.set_nonblocking(true)?;

    Ok(socket.into())
}