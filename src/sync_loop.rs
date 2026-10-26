//! dantesync#136 — the daemon's sync loop.
//!
//! One thread drives the PTP controller: `iterate` (`main`: the 10 s status tick and one
//! `process_loop_iteration`), then the platform's poll interval, until `running` clears.

use crate::status::SyncStatus;
use crate::time_server::TimeServer;
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
/// from `status`.
pub fn run<F: FnMut()>(
    time_server: Option<TimeServer>,
    status: Arc<RwLock<SyncStatus>>,
    running: Arc<AtomicBool>,
    mut iterate: F,
) {
    while running.load(Ordering::SeqCst) {
        iterate();

        // Handle UDP time query requests (for network time verification)
        if let Some(ref ts) = time_server {
            ts.handle_requests(&status);
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests;
