//! The Linux PTP receive path: two UDP sockets on the PTP ports (319 event, 320 general), joined
//! to the PTP multicast group `224.0.1.129` on the default interface, with kernel receive
//! timestamps (`SO_TIMESTAMPNS`, see `crate::net::create_multicast_socket`).
//!
//! It was `RealPtpNetwork` in `main.rs`. It lives here beside the other platform backends
//! (`net_pcap`, `net_winsock`) so it can be unit-tested through its [`PtpSocketFactory`] seam:
//! the interface resolver and the socket opener are its two OS boundaries.

use crate::net;
use crate::ptp::{PTP_EVENT_PORT, PTP_GENERAL_PORT};
use crate::traits::{PtpNetwork, RejoinOutcome};
use anyhow::{anyhow, Result};
use log::info;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::SystemTime;

/// The two OS boundaries of the Linux PTP receive path.
pub trait PtpSocketFactory {
    /// The interface to join the PTP group on, as `(name, IPv4)`, resolved now.
    fn resolve_interface(&mut self) -> Result<(String, Ipv4Addr)>;
    /// One non-blocking PTP socket bound to `port` and joined to the PTP multicast group on `ip`.
    fn open(&mut self, port: u16, ip: Ipv4Addr) -> Result<UdpSocket>;
}

/// The production factory: the default-interface resolver and the kernel-timestamped multicast
/// socket of `crate::net`.
pub struct KernelSockets;

impl PtpSocketFactory for KernelSockets {
    fn resolve_interface(&mut self) -> Result<(String, Ipv4Addr)> {
        net::get_default_interface()
    }

    fn open(&mut self, port: u16, ip: Ipv4Addr) -> Result<UdpSocket> {
        net::create_multicast_socket(port, ip)
    }
}

/// The event (319) and general (320) sockets of one join, and the interface they joined on.
struct Joined {
    event: UdpSocket,
    general: UdpSocket,
    iface: String,
    ip: Ipv4Addr,
}

/// Open the event and the general socket on `ip`. Either failing drops both.
fn open_pair<F: PtpSocketFactory>(factory: &mut F, iface: String, ip: Ipv4Addr) -> Result<Joined> {
    let event = factory.open(PTP_EVENT_PORT, ip)?;
    let general = factory.open(PTP_GENERAL_PORT, ip)?;
    Ok(Joined {
        event,
        general,
        iface,
        ip,
    })
}

/// Read and discard everything queued on `sock`.
fn drain(sock: &UdpSocket) {
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok(_) => continue,
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

/// Linux PTP network: kernel-timestamped UDP multicast sockets.
pub struct UdpPtpNetwork<F: PtpSocketFactory = KernelSockets> {
    factory: F,
    joined: Option<Joined>,
}

impl<F: PtpSocketFactory> UdpPtpNetwork<F> {
    /// Join on an interface the caller already resolved (startup waits for one with its own
    /// retry loop).
    pub fn join(mut factory: F, iface: String, ip: Ipv4Addr) -> Result<Self> {
        let joined = open_pair(&mut factory, iface, ip)?;
        info!(
            "Joined Multicast Groups on {} ({}) - Kernel timestamping",
            joined.iface, joined.ip
        );
        Ok(UdpPtpNetwork {
            factory,
            joined: Some(joined),
        })
    }

    /// The interface the sockets are joined on.
    pub fn interface(&self) -> Option<(&str, Ipv4Addr)> {
        self.joined.as_ref().map(|j| (j.iface.as_str(), j.ip))
    }
}

impl<F: PtpSocketFactory> PtpNetwork for UdpPtpNetwork<F> {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime, Option<Ipv4Addr>)>> {
        let Some(joined) = self.joined.as_ref() else {
            return Ok(None);
        };
        let mut buf = [0u8; 2048];

        // Check Event Socket first
        if let Some((size, ts, source_ip)) = net::recv_with_timestamp(&joined.event, &mut buf)? {
            return Ok(Some((buf[..size].to_vec(), size, ts, source_ip)));
        }

        // Check General Socket
        if let Some((size, ts, source_ip)) = net::recv_with_timestamp(&joined.general, &mut buf)? {
            return Ok(Some((buf[..size].to_vec(), size, ts, source_ip)));
        }

        Ok(None)
    }

    fn reset(&mut self) -> Result<()> {
        // Drain buffers to prevent processing old packets after a clock step
        if let Some(joined) = self.joined.as_ref() {
            drain(&joined.event);
            drain(&joined.general);
        }
        Ok(())
    }

    fn rejoin(&mut self) -> Result<RejoinOutcome> {
        Err(anyhow!(
            "PTP re-join is not implemented yet (dantesync#112)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::time::Duration;

    /// The two OS boundaries, scripted. The resolver answers from a queue. The opener binds a
    /// REAL non-blocking UDP socket on loopback (319/320 need root, so an ephemeral port) and can
    /// be told to fail on one port. Every socket gets its own loopback address (127.0.1.N), so a
    /// released address can never be taken again by a later socket of the same test.
    #[derive(Default)]
    struct ScriptedSockets {
        interfaces: VecDeque<Result<(String, Ipv4Addr)>>,
        fail_port: Option<u16>,
        /// Sockets bound so far (the next one binds 127.0.1.`binds + 1`).
        binds: u8,
        /// `(port, ip, the socket's own address)` of every socket opened.
        opened: Vec<(u16, Ipv4Addr, SocketAddr)>,
        /// Addresses that must already be released whenever a socket is opened.
        released_before_open: Vec<SocketAddr>,
        /// For every socket opened: were all of `released_before_open` free at that moment?
        free_at_open: Vec<bool>,
    }

    impl PtpSocketFactory for ScriptedSockets {
        fn resolve_interface(&mut self) -> Result<(String, Ipv4Addr)> {
            self.interfaces
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("No suitable IPv4 interface found")))
        }

        fn open(&mut self, port: u16, ip: Ipv4Addr) -> Result<UdpSocket> {
            let all_free = self.released_before_open.iter().all(|&a| is_free(a));
            self.free_at_open.push(all_free);
            if self.fail_port == Some(port) {
                return Err(anyhow!("cannot join the PTP group on {ip}: No such device"));
            }
            self.binds += 1;
            let sock = UdpSocket::bind((Ipv4Addr::new(127, 0, 1, self.binds), 0))?;
            sock.set_nonblocking(true)?;
            self.opened.push((port, ip, sock.local_addr()?));
            Ok(sock)
        }
    }

    const ETH0: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 202);
    const ETH1: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 203);

    fn joined(script: ScriptedSockets) -> UdpPtpNetwork<ScriptedSockets> {
        UdpPtpNetwork::join(script, "eth0".to_string(), ETH0).expect("the startup join")
    }

    /// The address can be bound again: no socket holds it any more.
    fn is_free(addr: SocketAddr) -> bool {
        UdpSocket::bind(addr).is_ok()
    }

    fn addrs(net: &UdpPtpNetwork<ScriptedSockets>) -> Vec<SocketAddr> {
        net.factory.opened.iter().map(|o| o.2).collect()
    }

    /// Send one datagram to `to` and read it back through the network (whichever socket).
    fn round_trip(
        net: &mut UdpPtpNetwork<ScriptedSockets>,
        to: SocketAddr,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("sender");
        tx.send_to(payload, to).expect("send");
        for _ in 0..500 {
            if let Some((data, len, _, src)) = net.recv_packet().expect("recv") {
                assert_eq!(len, payload.len());
                assert_eq!(src, Some(Ipv4Addr::LOCALHOST));
                return Some(data);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        None
    }

    #[test]
    fn the_startup_join_opens_the_event_then_the_general_socket_and_both_receive_112() {
        let mut net = joined(ScriptedSockets::default());
        let ports: Vec<(u16, Ipv4Addr)> = net.factory.opened.iter().map(|o| (o.0, o.1)).collect();
        assert_eq!(ports, vec![(319, ETH0), (320, ETH0)]);
        assert_eq!(net.interface(), Some(("eth0", ETH0)));
        let a = addrs(&net);
        assert_eq!(
            round_trip(&mut net, a[0], b"sync").as_deref(),
            Some(&b"sync"[..])
        );
        assert_eq!(
            round_trip(&mut net, a[1], b"fup").as_deref(),
            Some(&b"fup"[..])
        );
        assert_eq!(
            net.recv_packet().expect("recv"),
            None,
            "nothing else queued"
        );
    }

    #[test]
    fn reset_drains_both_sockets_112() {
        let mut net = joined(ScriptedSockets::default());
        let a = addrs(&net);
        let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("sender");
        tx.send_to(b"old sync", a[0]).expect("send");
        tx.send_to(b"old fup", a[1]).expect("send");
        std::thread::sleep(Duration::from_millis(20));
        net.reset().expect("reset");
        assert_eq!(net.recv_packet().expect("recv"), None);
    }

    #[test]
    fn a_rejoin_re_resolves_drops_both_sockets_first_and_joins_again_112() {
        let mut script = ScriptedSockets::default();
        script.interfaces.push_back(Ok(("eth1".to_string(), ETH1)));
        let mut net = joined(script);
        let old = addrs(&net);
        net.factory.released_before_open = old.clone();

        let out = net.rejoin().expect("the re-join");
        assert_eq!(
            out,
            RejoinOutcome {
                iface: "eth1".to_string(),
                ip: ETH1,
                changed: true
            }
        );
        assert_eq!(net.interface(), Some(("eth1", ETH1)));
        let ports: Vec<(u16, Ipv4Addr)> = net.factory.opened.iter().map(|o| (o.0, o.1)).collect();
        assert_eq!(
            ports,
            vec![(319, ETH0), (320, ETH0), (319, ETH1), (320, ETH1)],
            "the event and the general socket, joined again on the interface resolved NOW"
        );
        assert_eq!(
            net.factory.free_at_open[2..].to_vec(),
            vec![true, true],
            "both old sockets (and their membership) were closed BEFORE the new ones opened"
        );
        let new = addrs(&net);
        assert_eq!(
            round_trip(&mut net, new[2], b"sync").as_deref(),
            Some(&b"sync"[..])
        );
        assert_eq!(
            round_trip(&mut net, new[3], b"fup").as_deref(),
            Some(&b"fup"[..])
        );
    }

    #[test]
    fn a_rejoin_on_the_same_interface_still_recreates_the_sockets_and_reports_no_change_112() {
        // The 29.9.2026 strih-lx case: the USB NIC came back under the same name and IP, but as a
        // new netdev, and the old membership was gone with the old one.
        let mut script = ScriptedSockets::default();
        script.interfaces.push_back(Ok(("eth0".to_string(), ETH0)));
        let mut net = joined(script);
        net.factory.released_before_open = addrs(&net);
        let out = net.rejoin().expect("the re-join");
        assert_eq!(
            out,
            RejoinOutcome {
                iface: "eth0".to_string(),
                ip: ETH0,
                changed: false
            }
        );
        assert_eq!(net.factory.opened.len(), 4, "two new sockets all the same");
        assert_eq!(
            net.factory.free_at_open[2..].to_vec(),
            vec![true, true],
            "the old pair was closed first"
        );
    }

    #[test]
    fn a_rejoin_that_cannot_resolve_an_interface_keeps_the_old_sockets_112() {
        let mut net = joined(ScriptedSockets::default()); // nothing scripted: resolve fails
        let old = addrs(&net);
        let err = net.rejoin().expect_err("no interface");
        assert!(
            err.to_string().contains("No suitable IPv4 interface"),
            "{err}"
        );
        assert_eq!(net.interface(), Some(("eth0", ETH0)), "still joined");
        assert_eq!(net.factory.opened.len(), 2, "nothing opened");
        assert!(
            !is_free(old[0]) && !is_free(old[1]),
            "the old pair is still open"
        );
        assert_eq!(
            round_trip(&mut net, old[0], b"sync").as_deref(),
            Some(&b"sync"[..])
        );
    }

    #[test]
    fn a_rejoin_that_cannot_open_leaves_no_sockets_until_the_next_attempt_112() {
        let mut script = ScriptedSockets::default();
        script.interfaces.push_back(Ok(("eth1".to_string(), ETH1)));
        script.interfaces.push_back(Ok(("eth1".to_string(), ETH1)));
        script.fail_port = Some(320);
        let mut net = joined_after_startup_failing_port(script);
        let old = addrs(&net);

        let err = net.rejoin().expect_err("the general socket cannot open");
        assert!(err.to_string().contains("No such device"), "{err}");
        assert_eq!(net.interface(), None, "no sockets");
        assert_eq!(net.recv_packet().expect("recv"), None, "nothing to read");
        let after = addrs(&net);
        assert_eq!(
            after.len(),
            3,
            "the failed attempt opened only the event socket"
        );
        assert!(
            old.iter().all(|&a| is_free(a)) && is_free(after[2]),
            "the old pair and the failed attempt's event socket are all closed"
        );

        // The next attempt (on the controller's schedule) works.
        net.factory.fail_port = None;
        let out = net.rejoin().expect("the second re-join");
        assert!(
            out.changed,
            "compared with the last join that worked (eth0)"
        );
        assert_eq!(net.interface(), Some(("eth1", ETH1)));
        let new = addrs(&net);
        assert_eq!(
            round_trip(&mut net, new[4], b"fup").as_deref(),
            Some(&b"fup"[..])
        );
    }

    /// The startup join works; only later opens fail on the scripted port.
    fn joined_after_startup_failing_port(
        mut script: ScriptedSockets,
    ) -> UdpPtpNetwork<ScriptedSockets> {
        let fail = script.fail_port.take();
        let mut net = joined(script);
        net.factory.fail_port = fail;
        net
    }
}
