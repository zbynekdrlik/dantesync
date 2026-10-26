//! dantesync#136 — the periodic NTP burst, measured off the sync loop.
//!
//! Before 1.20 the loop's periodic NTP check ran its burst (`NTP_BURST_SIZE` sequential queries,
//! `NtpClient::get_offset`) inline: PTP processing paused for the whole burst — longer under load
//! on the NIC — and so did every time query. Now the burst runs on its own `ntp-burst` thread
//! ([`NtpBurstThread`]) and the loop takes the finished result when it is there
//! ([`NtpBurstRunner::take_result`], a channel it never blocks on). The cadence, the timeouts and
//! the burst filter are unchanged: the thread runs exactly the burst the loop asked for, when it
//! asked for it, with the same client.

use crate::ntp::NtpMeasurement;
use anyhow::{anyhow, Result};
use log::{error, warn};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How often an idle `ntp-burst` thread looks at `running` (its shutdown latency when idle).
pub const BURST_THREAD_IDLE_POLL: Duration = Duration::from_millis(100);

/// Where the controller has its periodic NTP burst measured (`main`: [`NtpBurstThread`]; a test
/// injects a scripted one). Without one the controller measures inline, as before 1.20.
pub trait NtpBurstRunner {
    /// Start one burst now. `false` when nothing was started (one is still running, or the
    /// thread has ended).
    fn start(&mut self) -> bool;
    /// The finished burst, once; `None` while it runs. Never blocks.
    fn take_result(&mut self) -> Option<Result<NtpMeasurement>>;
}

/// The burst on a thread of its own (`ntp-burst`): one request in, one result out.
pub struct NtpBurstThread {
    requests: Sender<()>,
    results: Receiver<Result<NtpMeasurement>>,
    in_flight: bool,
    /// Joined by the tests only: the daemon never waits for it at shutdown (a burst in flight
    /// may take seconds; the thread ends on its own once `running` clears).
    #[cfg_attr(not(test), allow(dead_code))]
    handle: Option<JoinHandle<()>>,
}

impl NtpBurstThread {
    /// Start the `ntp-burst` thread: it runs `measure` (one burst, `NtpClient::get_offset`) each
    /// time the loop asks for one, until `running` clears (a burst in flight is finished first;
    /// an idle thread notices within [`BURST_THREAD_IDLE_POLL`]) or the runner is dropped.
    pub fn spawn<F>(mut measure: F, running: Arc<AtomicBool>) -> std::io::Result<Self>
    where
        F: FnMut() -> Result<NtpMeasurement> + Send + 'static,
    {
        let (requests, request_rx) = mpsc::channel::<()>();
        let (result_tx, results) = mpsc::channel::<Result<NtpMeasurement>>();
        let handle = std::thread::Builder::new()
            .name("ntp-burst".to_string())
            .spawn(move || {
                while running.load(Ordering::SeqCst) {
                    match request_rx.recv_timeout(BURST_THREAD_IDLE_POLL) {
                        Ok(()) => {
                            // A panic inside one burst is that burst's failure, never the end of
                            // the periodic NTP (the next check measures again).
                            let result = catch_unwind(AssertUnwindSafe(&mut measure))
                                .unwrap_or_else(|panic| {
                                    let what = panic_text(panic.as_ref());
                                    error!("[NTP] the NTP burst panicked: {}", what);
                                    Err(anyhow!("the NTP burst panicked: {}", what))
                                });
                            if result_tx.send(result).is_err() {
                                break; // the controller is gone
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok(NtpBurstThread {
            requests,
            results,
            in_flight: false,
            handle: Some(handle),
        })
    }

    /// Wait for the thread to end (it ends once `running` clears, after a burst in flight).
    #[cfg(test)]
    pub(crate) fn join(self) -> std::thread::Result<()> {
        self.joined().map(|_| ())
    }

    /// Wait for the thread to end and keep the (now thread-less) runner.
    #[cfg(test)]
    pub(crate) fn joined(mut self) -> std::thread::Result<Self> {
        if let Some(handle) = self.handle.take() {
            handle.join()?;
        }
        Ok(self)
    }
}

/// A panic payload as text (a `&str` or a `String`, as `panic!` makes them).
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "(no message)".to_string()
    }
}

impl NtpBurstRunner for NtpBurstThread {
    fn start(&mut self) -> bool {
        if self.in_flight {
            return false;
        }
        if self.requests.send(()).is_err() {
            warn!("[NTP] the ntp-burst thread has ended — no NTP burst");
            return false;
        }
        self.in_flight = true;
        true
    }

    fn take_result(&mut self) -> Option<Result<NtpMeasurement>> {
        if !self.in_flight {
            return None;
        }
        match self.results.try_recv() {
            Ok(result) => {
                self.in_flight = false;
                Some(result)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.in_flight = false;
                Some(Err(anyhow!("the ntp-burst thread ended during a burst")))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
