//! dantesync#136 — the daemon's sync loop and its time server.
//!
//! One thread drives the PTP controller: `iterate` (`main`: the 10 s status tick and one
//! `process_loop_iteration`), then the platform's poll interval, until `running` clears. The UDP
//! 31900 time server answers on a thread of its own (`TimeServer::spawn`) from the latest status
//! snapshot, started with the loop and joined when it ends: a time query never waits for an
//! iteration (before 1.20 the loop answered between its iterations, behind PTP processing and the
//! NTP burst).

use crate::status::SyncStatus;
use crate::time_server::TimeServer;
use log::{error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The loop's poll interval:
/// - Windows: 50µs tight polling for lower jitter with software timestamps.
///   This achieves ~5% CPU usage while maintaining <50µs precision.
///   Tested across Intel/AMD systems; tighter values increase CPU without benefit.
/// - Linux: 1ms is sufficient since we use kernel SO_TIMESTAMPNS which provides
///   accurate timestamps regardless of polling frequency.
#[cfg(windows)]
pub const POLL_INTERVAL: Duration = Duration::from_micros(50);
#[cfg(not(windows))]
pub const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Run the sync loop until `running` clears. `time_server` (the UDP 31900 time queries) answers
/// from `status` on its own thread, started here (after `main` raised the process to real-time
/// priority, which the thread inherits like the loop) and ended with the loop (within
/// `TIME_SERVER_READ_TIMEOUT` of `running` clearing).
pub fn run<F: FnMut()>(
    time_server: Option<TimeServer>,
    status: Arc<RwLock<SyncStatus>>,
    running: Arc<AtomicBool>,
    mut iterate: F,
) {
    let server_thread = time_server.and_then(|ts| match ts.spawn(status, running.clone()) {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!(
                "[TimeServer] Could not start its thread: {} (continuing without it)",
                e
            );
            None
        }
    });

    while running.load(Ordering::SeqCst) {
        iterate();
        std::thread::sleep(POLL_INTERVAL);
    }

    if let Some(handle) = server_thread {
        if handle.join().is_err() {
            error!("[TimeServer] its thread ended with a panic");
        }
    }
}

#[cfg(test)]
mod tests;
