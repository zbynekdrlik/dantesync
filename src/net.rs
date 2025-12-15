use anyhow::{Result, anyhow};
use pnet_datalink::{self, NetworkInterface};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(NetworkInterface, Ipv4Addr)> {
    let interfaces = pnet_datalink::interfaces();
    let usable_interfaces: Vec<&NetworkInterface> = interfaces.iter()
        .filter(|iface| iface.is_up() && !iface.is_loopback() && !iface.ips.is_empty())
        .collect();

    if usable_interfaces.is_empty() {
        return Err(anyhow!("No suitable network interface found"));
    }

    let mut best_iface = None;
    let mut best_ip = None;

    for iface in usable_interfaces {
        let ipv4 = iface.ips.iter().find(|ip| ip.is_ipv4()).map(|ip| {
             if let IpAddr::V4(addr) = ip.ip() { addr } else { unreachable!() }
        });

        if let Some(ip) = ipv4 {
            let name_lower = iface.name.to_lowercase();
            let is_likely_wireless = name_lower.contains("wlan") || name_lower.contains("wifi") || name_lower.contains("wireless");
            
            if !is_likely_wireless {
                best_iface = Some(iface.clone());
                best_ip = Some(ip);
                break;
            } else if best_iface.is_none() {
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
    
    socket.set_reuse_address(true)?;
    
    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    let multi_addr: Ipv4Addr = "224.0.1.129".parse()?;
    socket.join_multicast_v4(&multi_addr, &interface_ip)?;
    
    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    let udp_socket: UdpSocket = socket.into();

    #[cfg(unix)]
    {
        // Enable Kernel Timestamping (SO_TIMESTAMPNS)
        // Pass &udp_socket which implements AsFd
        match setsockopt(&udp_socket, sockopt::ReceiveTimestampns, &true) {
            Ok(_) => log::info!("Kernel timestamping (SO_TIMESTAMPNS) enabled."),
            Err(e) => log::warn!("Failed to enable kernel timestamping: {}", e),
        }
    }

    Ok(udp_socket)
}