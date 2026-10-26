//! dantesync#136 — the periodic NTP burst, measured off the sync loop.
//!
//! RED stub (the thread is not there yet): nothing is started, nothing is delivered.

use crate::ntp::NtpMeasurement;
use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Where the controller has its periodic NTP burst measured (`main`: [`NtpBurstThread`]; a test
/// injects a scripted one). Without one the controller measures inline, as before 1.20.
pub trait NtpBurstRunner {
    /// Start one burst now. `false` when nothing was started (one is still running, or the
    /// thread has ended).
    fn start(&mut self) -> bool;
    /// The finished burst, once; `None` while it runs. Never blocks.
    fn take_result(&mut self) -> Option<Result<NtpMeasurement>>;
}

/// The burst on a thread of its own (`ntp-burst`).
pub struct NtpBurstThread {
    _stub: (),
}

impl NtpBurstThread {
    /// Start the `ntp-burst` thread: it runs `measure` (one burst, `NtpClient::get_offset`) each
    /// time the loop asks for one, until `running` clears.
    pub fn spawn<F>(measure: F, running: Arc<AtomicBool>) -> std::io::Result<Self>
    where
        F: FnMut() -> Result<NtpMeasurement> + Send + 'static,
    {
        let _ = (measure, running);
        Ok(NtpBurstThread { _stub: () })
    }

    /// Wait for the thread to end (it ends once `running` clears, after a burst in flight).
    #[cfg(test)]
    pub(crate) fn join(self) -> std::thread::Result<()> {
        Ok(())
    }

    /// Wait for the thread to end and keep the (now thread-less) runner.
    #[cfg(test)]
    pub(crate) fn joined(self) -> std::thread::Result<Self> {
        Ok(self)
    }
}

impl NtpBurstRunner for NtpBurstThread {
    fn start(&mut self) -> bool {
        false
    }

    fn take_result(&mut self) -> Option<Result<NtpMeasurement>> {
        None
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
