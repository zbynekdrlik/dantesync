//! The Linux PTP receive path: two UDP sockets on the PTP ports (319 event, 320 general), joined
//! to the PTP multicast group `224.0.1.129` on the default interface, with kernel receive
//! timestamps (`SO_TIMESTAMPNS`, see `crate::net::create_multicast_socket`).
//!
//! It was `RealPtpNetwork` in `main.rs`. It lives here beside the other platform backends
//! (`net_pcap`, `net_winsock`) so it can be unit-tested through its [`PtpSocketFactory`] seam:
//! the interface resolver and the socket opener are its two OS boundaries.

use crate::net;
use crate::ptp::{PTP_EVENT_PORT, PTP_GENERAL_PORT};
use crate::traits::PtpNetwork;
use anyhow::Result;
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
}
