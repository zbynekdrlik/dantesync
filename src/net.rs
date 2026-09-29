use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let ifaces = if_addrs::get_if_addrs()?;

    let mut best_iface = None;

    for iface in &ifaces {
        // Skip loopback and non-IPv4
        let ip = match iface.addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => ip,
            _ => continue,
        };

        // Skip wireless interfaces if possible
        let name_lower = iface.name.to_lowercase();
        let is_wireless = name_lower.contains("wireless")
            || name_lower.contains("wi-fi")
            || name_lower.contains("wlan");

        // Verify we can actually bind to this IP
        if is_ip_bindable(ip) {
            if !is_wireless {
                return Ok((iface.name.clone(), ip));
            } else if best_iface.is_none() {
                best_iface = Some((iface.name.clone(), ip));
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for iface in &ifaces {
        log::warn!(" - Name: {}, Addr: {:?}", iface.name, iface.addr);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
}

/// dantesync#112 — the name of the interface that carries exactly `ip` now (never loopback), if
/// any. The PTP re-join looks for the NIC it last received PTP on this way: a NIC that is re-plugged
/// (USB) comes back with a new, higher ifindex, often under a new name, so the listing-ordered
/// [`get_default_interface`] may put another interface (tailscale, docker, a bridge) first.
pub fn interface_with_ip(ip: Ipv4Addr) -> Option<String> {
    if ip.is_loopback() {
        return None;
    }
    let ifaces = if_addrs::get_if_addrs().ok()?;
    ifaces
        .into_iter()
        .find(|iface| iface.ip() == IpAddr::V4(ip))
        .map(|iface| iface.name)
}

/// dantesync#112 — the Windows PTP capture choice, pure so Linux CI tests it. `by_rule` is
/// `net_pcap::find_ptp_capture_device`'s answer: `Ok((device, Some(ip)))` when the trusted
/// grandmaster subnet of `gm_allowlist` picks one device (camera-box issue 1073; it always wins),
/// `Ok((device, None))` for the name fallback. `home` is the device that carries the address PTP
/// was last received on, when a re-join looks for one (startup passes `None`): after a NIC swap
/// the adapter comes back as another device under another name, so it comes before the name
/// fallback. Returns the device and the address to join on (`None` = the device's first IPv4).
pub fn choose_capture_device<D>(
    by_rule: Result<(D, Option<Ipv4Addr>)>,
    home: Option<(D, Ipv4Addr)>,
) -> Result<(D, Option<Ipv4Addr>)> {
    match by_rule {
        Ok((device, Some(ip))) => Ok((device, Some(ip))),
        by_name => match home {
            Some((device, ip)) => Ok((device, Some(ip))),
            None => by_name,
        },
    }
}

/// dantesync#112 — whether [`choose_capture_device`] needs the home device looked up: only on a
/// re-join (`home` is `Some`) whose trusted-subnet rule found no unique device (it would win
/// anyway). Returns the address to look the device up by.
pub fn home_to_look_up<D>(
    by_rule: &Result<(D, Option<Ipv4Addr>)>,
    home: Option<Ipv4Addr>,
) -> Option<Ipv4Addr> {
    match by_rule {
        Ok((_, Some(_))) => None,
        _ => home,
    }
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

/// The bind address for the pcap IGMP-join socket (dantesync#109).
///
/// The Windows pcap path opens a UDP socket purely to trigger the kernel IGMP
/// membership report for the PTP group `224.0.1.129`. IGMP membership is per
/// interface+group, NOT per port, so the socket's bound source port is
/// irrelevant to the join (and pcap's own BPF filter, not this socket, selects
/// the captured traffic). It must therefore bind an EPHEMERAL port — never a
/// PTP port (319/320), which on a Dante Virtual Soundcard host belong to DVS's
/// own `ptp.exe`: dantesync (a boot-time service) bound them first, so
/// `ptp.exe` (started later, without `SO_REUSEADDR`) failed its bind with
/// WSAEADDRINUSE and its PTP follower was starved of Follow_Up/Delay_Resp on
/// 320.
pub fn igmp_join_bind_addr() -> SocketAddrV4 {
    // dantesync#109: bind an EPHEMERAL port (0). The kernel picks a free source
    // port for the join socket; 319/320 stay free for a DVS ptp.exe on the same
    // host. The port is irrelevant to the IGMP membership and to pcap's capture.
    SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)
}

#[cfg(unix)]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime, Option<Ipv4Addr>)>> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};
    use nix::sys::time::TimeSpec;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime};

    let fd = sock.as_raw_fd();
    let mut iov = [std::io::IoSliceMut::new(buf)];
    let mut cmsg_buf = nix::cmsg_space!(TimeSpec);

    match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
        Ok(msg) => {
            let timestamp = msg
                .cmsgs()
                .find_map(|cmsg| {
                    if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                        let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                        Some(SystemTime::UNIX_EPOCH + duration)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(SystemTime::now);

            // Extract source IP from the address field
            let source_ip = msg.address.and_then(|addr| {
                addr.as_sockaddr_in().map(|sin| {
                    let ip = sin.ip();
                    Ipv4Addr::new(
                        ((ip >> 24) & 0xFF) as u8,
                        ((ip >> 16) & 0xFF) as u8,
                        ((ip >> 8) & 0xFF) as u8,
                        (ip & 0xFF) as u8,
                    )
                })
            });

            Ok(Some((msg.bytes, timestamp, source_ip)))
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime, Option<Ipv4Addr>)>> {
    match sock.recv_from(buf) {
        Ok((size, addr)) => {
            let source_ip = match addr {
                std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
                _ => None,
            };
            Ok(Some((size, std::time::SystemTime::now(), source_ip)))
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that get_default_interface filters out loopback addresses
    #[test]
    fn test_get_default_interface_returns_non_loopback() {
        // This test verifies the interface selection logic runs without panic
        // On systems with valid network interfaces, it should succeed
        // On systems without interfaces, it returns an error (which is valid)
        let result = get_default_interface();
        if let Ok((name, ip)) = result {
            assert!(!name.is_empty(), "Interface name should not be empty");
            assert!(!ip.is_loopback(), "Should not return loopback address");
        }
        // Error case is acceptable on minimal test environments
    }

    /// dantesync#112: the interface carrying an address is found by that address.
    #[test]
    fn interface_with_ip_finds_the_interface_by_its_address_112() {
        if let Ok((name, ip)) = get_default_interface() {
            assert_eq!(interface_with_ip(ip), Some(name));
        }
        assert_eq!(
            interface_with_ip(Ipv4Addr::new(192, 0, 2, 1)),
            None,
            "TEST-NET-1 is on no interface"
        );
        assert_eq!(
            interface_with_ip(Ipv4Addr::LOCALHOST),
            None,
            "never loopback"
        );
    }

    const HOME: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 204);

    #[test]
    fn the_trusted_subnet_rule_wins_over_the_home_device_112() {
        let rule = Ok(("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
        let got = choose_capture_device(rule, Some(("old-home-nic", HOME))).expect("a device");
        assert_eq!(got, ("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
    }

    #[test]
    fn the_home_device_wins_over_the_name_fallback_112() {
        let by_name = Ok(("default-nic", None));
        let got = choose_capture_device(by_name, Some(("swapped-nic", HOME))).expect("a device");
        assert_eq!(got, ("swapped-nic", Some(HOME)));
    }

    #[test]
    fn the_home_device_is_used_when_the_name_fallback_found_nothing_112() {
        let by_name: Result<(&str, Option<Ipv4Addr>)> =
            Err(anyhow!("Interface 'Ethernet' not found"));
        let got = choose_capture_device(by_name, Some(("swapped-nic", HOME))).expect("a device");
        assert_eq!(got, ("swapped-nic", Some(HOME)));
    }

    #[test]
    fn the_home_device_is_looked_up_only_on_a_re_join_without_a_trusted_subnet_match_112() {
        let trusted: Result<(&str, Option<Ipv4Addr>)> =
            Ok(("rig-nic", Some(Ipv4Addr::new(10, 77, 9, 205))));
        let by_name: Result<(&str, Option<Ipv4Addr>)> = Ok(("default-nic", None));
        let none: Result<(&str, Option<Ipv4Addr>)> = Err(anyhow!("not found"));
        assert_eq!(
            home_to_look_up(&trusted, Some(HOME)),
            None,
            "the rule wins anyway"
        );
        assert_eq!(home_to_look_up(&by_name, Some(HOME)), Some(HOME));
        assert_eq!(home_to_look_up(&none, Some(HOME)), Some(HOME));
        assert_eq!(
            home_to_look_up(&by_name, None),
            None,
            "the startup has no home"
        );
        assert_eq!(home_to_look_up(&none, None), None);
    }

    #[test]
    fn without_a_home_device_the_startup_choice_stands_112() {
        let by_name = Ok(("default-nic", None));
        assert_eq!(
            choose_capture_device(by_name, None).expect("a device"),
            ("default-nic", None)
        );
        let none: Result<(&str, Option<Ipv4Addr>)> = Err(anyhow!("Interface 'Ethernet' not found"));
        assert!(
            choose_capture_device(none, None).is_err(),
            "nothing to join: the caller keeps its old capture"
        );
    }

    /// Test is_ip_bindable with loopback (should always work)
    #[test]
    fn test_is_ip_bindable_loopback() {
        // Loopback should always be bindable on any system
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        assert!(is_ip_bindable(loopback), "Loopback should be bindable");
    }

    /// Test is_ip_bindable with UNSPECIFIED address
    #[test]
    fn test_is_ip_bindable_unspecified() {
        // 0.0.0.0 should be bindable (binds to all interfaces)
        let unspecified = Ipv4Addr::UNSPECIFIED;
        assert!(
            is_ip_bindable(unspecified),
            "UNSPECIFIED (0.0.0.0) should be bindable"
        );
    }

    /// Test PTP multicast address constant
    #[test]
    fn test_ptp_multicast_address() {
        let multi_addr: Ipv4Addr = "224.0.1.129".parse().unwrap();
        assert!(multi_addr.is_multicast(), "PTP address should be multicast");
        assert_eq!(multi_addr.octets(), [224, 0, 1, 129]);
    }

    /// dantesync#109: the pcap IGMP-join socket must bind an EPHEMERAL port,
    /// never a PTP port. Binding 319/320 collides with a Dante Virtual
    /// Soundcard `ptp.exe` on the same host and starves its PTP follower of the
    /// PTP general messages (Follow_Up/Delay_Resp) on 320 — the DVS media clock
    /// then free-runs on the host crystal instead of the grandmaster.
    #[test]
    fn igmp_join_bind_addr_never_uses_a_ptp_port() {
        let addr = igmp_join_bind_addr();
        assert_eq!(
            addr.port(),
            0,
            "IGMP-join socket must bind an ephemeral port (0), not a PTP port"
        );
        assert_ne!(
            addr.port(),
            319,
            "must not bind PTP event port 319 (DVS ptp.exe needs it)"
        );
        assert_ne!(
            addr.port(),
            320,
            "must not bind PTP general port 320 (DVS ptp.exe needs it)"
        );
        assert_eq!(
            *addr.ip(),
            Ipv4Addr::UNSPECIFIED,
            "IGMP-join socket binds the unspecified address"
        );
    }

    /// dantesync#109: a real UDP socket bound the way the IGMP join binds lands
    /// on a concrete NON-zero ephemeral port and never on 319/320 — so it can
    /// never collide with an exclusive PTP-port listener (e.g. DVS `ptp.exe`).
    #[test]
    fn igmp_join_socket_binds_a_nonzero_ephemeral_port() {
        let sock =
            UdpSocket::bind(igmp_join_bind_addr()).expect("bind the ephemeral IGMP-join socket");
        let port = sock
            .local_addr()
            .expect("read local_addr of the join socket")
            .port();
        assert_ne!(port, 0, "kernel must assign a concrete ephemeral port");
        assert_ne!(port, 319, "the join socket must never land on PTP port 319");
        assert_ne!(port, 320, "the join socket must never land on PTP port 320");
    }

    /// Test recv_with_timestamp returns None for non-blocking socket with no data
    #[test]
    fn test_recv_with_timestamp_no_data() {
        // Create a simple non-blocking UDP socket
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();

        let udp_socket: UdpSocket = socket.into();
        let mut buf = [0u8; 512];

        let result = recv_with_timestamp(&udp_socket, &mut buf);
        assert!(result.is_ok());
        // Should return None since no data is available
        assert!(result.unwrap().is_none());
    }

    /// Test wireless interface detection keywords
    #[test]
    fn test_wireless_interface_detection() {
        // Test the wireless detection logic used in get_default_interface
        let wireless_names = ["Wireless LAN", "Wi-Fi", "wlan0", "WIRELESS"];
        let wired_names = ["eth0", "Ethernet", "enp3s0", "Local Area Connection"];

        for name in &wireless_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(is_wireless, "{} should be detected as wireless", name);
        }

        for name in &wired_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(!is_wireless, "{} should NOT be detected as wireless", name);
        }
    }
}
