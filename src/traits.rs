use crate::ntp::NtpMeasurement;
use anyhow::Result;
use std::net::Ipv4Addr;

#[cfg_attr(test, mockall::automock)]
pub trait NtpSource {
    /// dantesync#53: returns a burst-filtered `NtpMeasurement` (offset/sign
    /// plus quality fields), not a single raw round trip.
    fn get_offset(&self) -> Result<NtpMeasurement>;
}

/// dantesync#112 — where a [`PtpNetwork::rejoin`] left the PTP receive path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejoinOutcome {
    /// The interface the PTP group is joined on now: the interface name on Linux, the Npcap
    /// device name on Windows.
    pub iface: String,
    /// The IPv4 address the group is joined on.
    pub ip: Ipv4Addr,
    /// The interface or the address differs from the previous successful join.
    pub changed: bool,
}

#[cfg_attr(test, mockall::automock)]
pub trait PtpNetwork {
    /// Receive a packet. Returns Ok(Some((data, len, timestamp, source_ip))) if packet received.
    /// Returns Ok(None) if no packet (timeout/wouldblock).
    /// source_ip is the IP address of the device sending the PTP packet.
    #[allow(clippy::type_complexity)]
    fn recv_packet(
        &mut self,
    ) -> Result<Option<(Vec<u8>, usize, std::time::SystemTime, Option<Ipv4Addr>)>>;

    /// Reset the network state (e.g. clear buffers). Default impl does nothing.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }

    /// dantesync#112 — open the PTP receive path again, on the interface the startup selection
    /// picks NOW: re-resolve the interface, drop the old sockets / capture (and with them the
    /// multicast membership, which may belong to a netdev that is gone), then join again.
    ///
    /// The controller calls it when no allowed PTP packet has come for 10 s, then on a backoff
    /// (`crate::ptp_rejoin`). It never touches the clock. An `Err` when the interface cannot be
    /// resolved leaves the old path in place; an `Err` when the new one cannot be opened leaves
    /// none (nothing is received) until the next attempt. Either way the controller retries on
    /// its schedule.
    fn rejoin(&mut self) -> Result<RejoinOutcome>;
}
