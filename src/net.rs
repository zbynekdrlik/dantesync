use anyhow::{Result, anyhow};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use pcap::Device;

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let devices = Device::list()?;
    
    let valid_devices: Vec<_> = devices.iter()
        .filter(|d| !d.addresses.is_empty())
        .collect();

    if valid_devices.is_empty() {
        log::warn!("No network interfaces found via Pcap.");
        return Err(anyhow!("No suitable network interface found"));
    }

    let mut best_iface = None;
    
    for dev in valid_devices {
        // Find IPv4
        let ipv4 = dev.addresses.iter().find(|a| {
            // pcap::Address.addr is std::net::IpAddr
            match a.addr {
                IpAddr::V4(ip) => !ip.is_loopback(),
                _ => false,
            }
        });

        if let Some(ipv4_addr) = ipv4 {
            let ip = if let IpAddr::V4(addr) = ipv4_addr.addr {
                addr
            } else {
                continue;
            };

            // Prefer non-wireless/non-loopback
            // pcap::Device has `desc` field (Option<String>)
            let desc_str = dev.desc.as_deref().unwrap_or("").to_lowercase();
            let is_wireless = desc_str.contains("wireless") || desc_str.contains("wi-fi") || desc_str.contains("wlan");
            
            // Verify we can actually bind to this IP (WinSock check)
            if is_ip_bindable(ip) {
                if !is_wireless {
                    return Ok((dev.name.clone(), ip));
                } else if best_iface.is_none() {
                    best_iface = Some((dev.name.clone(), ip));
                }
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for dev in devices {
        log::warn!(" - Name: {}, Desc: {:?}, Addrs: {:?}", dev.name, dev.desc, dev.addresses);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
}

fn is_ip_bindable(ip: Ipv4Addr) -> bool {
    let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let addr = SocketAddrV4::new(ip, 0); // Port 0 (ephemeral)
    socket.bind(&addr.into()).is_ok()
}

pub fn create_multicast_socket(port: u16, interface_ip: Ipv4Addr) -> Result<UdpSocket> {
    // Standard UDP socket creation for TX (Transmission) or legacy RX
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
        match setsockopt(&udp_socket, sockopt::ReceiveTimestampns, &true) {
            Ok(_) => log::info!("Kernel timestamping (SO_TIMESTAMPNS) enabled."),
            Err(e) => log::warn!("Failed to enable kernel timestamping: {}", e),
        }
    }

    Ok(udp_socket)
}

#[cfg(unix)]
pub fn recv_with_timestamp(sock: &UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, std::time::SystemTime)>> {
    use std::os::fd::AsRawFd;
    use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, SockaddrStorage};
    use nix::sys::time::TimeSpec;
    use std::time::{Duration, SystemTime};

    let fd = sock.as_raw_fd();
    let mut iov = [std::io::IoSliceMut::new(buf)];
    let mut cmsg_buf = nix::cmsg_space!(TimeSpec);
    
    match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
        Ok(msg) => {
            let timestamp = msg.cmsgs().find_map(|cmsg| {
                if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                    let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                    Some(SystemTime::UNIX_EPOCH + duration)
                } else {
                    None
                }
            }).unwrap_or_else(SystemTime::now);
            
            Ok(Some((msg.bytes, timestamp)))
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub fn recv_with_timestamp(sock: &UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, std::time::SystemTime)>> {
    match sock.recv_from(buf) {
        Ok((size, _)) => Ok(Some((size, std::time::SystemTime::now()))),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}