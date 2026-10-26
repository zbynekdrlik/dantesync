//! dantesync#134 — fast boot sync: the boot NTP reading arrives on a channel, the PTP loop never
//! waits for it.

use super::*;
use crate::ntp::NtpMeasurement;
use std::sync::mpsc::Receiver;

/// One boot NTP reading, measured off the loop (`main`'s boot-NTP thread).
pub type BootNtpResult = std::result::Result<NtpMeasurement, String>;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// `main`: the boot NTP reading will arrive on `rx`.
    pub fn start_boot_ntp(&mut self, rx: Receiver<BootNtpResult>) {
        let _ = rx;
    }

    /// Every loop iteration: apply the boot NTP reading once it has arrived.
    pub fn service_boot_ntp(&mut self) {}
}
