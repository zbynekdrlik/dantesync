use crate::ntp::NtpMeasurement;
use anyhow::Result;
use std::net::Ipv4Addr;

#[cfg_attr(test, mockall::automock)]
pub trait NtpSource {
    /// dantesync#53: returns a burst-filtered `NtpMeasurement` (offset/sign
    /// plus quality fields), not a single raw round trip.
    fn get_offset(&self) -> Result<NtpMeasurement>;
}

#[cfg_attr(test, mockall::automock)]
pub trait PtpNetwork {
    /// Receive a packet. Returns Ok(Some((data, len, timestamp, source_ip))) if packet received.
    /// Returns Ok(None) if no packet (timeout/wouldblock).
    /// source_ip is the IP address of the device that sent the PTP packet.
    #[allow(clippy::type_complexity)]
    fn recv_packet(
        &mut self,
    ) -> Result<Option<(Vec<u8>, usize, std::time::SystemTime, Option<Ipv4Addr>)>>;

    /// Reset the network state (e.g. clear buffers). Default impl does nothing.
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
}
