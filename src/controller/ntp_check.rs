//! dantesync#136 — the periodic NTP check with its burst off the sync loop.
//!
//! RED stub: the runner is not used yet (the burst still runs inline).

use super::*;
use crate::ntp_burst::NtpBurstRunner;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// `main`: where the periodic NTP burst is measured (the `ntp-burst` thread).
    pub fn set_ntp_burst_runner(&mut self, runner: Box<dyn NtpBurstRunner>) {
        let _ = runner;
    }

    /// Every loop iteration: apply the periodic burst's result once it has arrived.
    pub(in crate::controller) fn service_ntp_burst(&mut self) {}
}

#[cfg(test)]
mod tests;
