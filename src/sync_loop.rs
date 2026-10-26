//! dantesync#136 — the daemon's sync loop and its time server.
//!
//! One thread drives the PTP controller: `iterate` (`main`: the 10 s status tick and one
//! `process_loop_iteration`), then the platform's poll interval, until `running` clears. The UDP
//! 31900 time server answers on a thread of its own (`TimeServer::spawn`) from the latest status
//! snapshot, started with the loop and joined when it ends: a time query never waits for an
//! iteration (before 1.20 the loop answered between its iterations, behind PTP processing and the
//! NTP burst). It answers only while the loop runs: never once the loop has ended (also by a
//! panic), never while it has not completed an iteration for
//! [`crate::time_server::STATUS_STALE_AFTER`] (the snapshot is the loop's).

use crate::status::SyncStatus;
use crate::time_server::TimeServer;
use log::{error, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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

/// dantesync#136 review — when the sync loop last completed an iteration (monotonic: this daemon
/// steps its own wall clock). The time server answers only while it is recent
/// ([`crate::time_server::STATUS_STALE_AFTER`]): the status snapshot it answers from is the loop's.
#[derive(Debug)]
pub struct LoopHeartbeat {
    origin: Instant,
    /// The last beat, µs after `origin`.
    last_us: AtomicU64,
}

impl LoopHeartbeat {
    /// A heartbeat that beat just now.
    pub fn new() -> Self {
        LoopHeartbeat {
            origin: Instant::now(),
            last_us: AtomicU64::new(0),
        }
    }

    /// A heartbeat whose last beat was `ago` (a test's stale loop).
    #[cfg(test)]
    pub(crate) fn last_beat_ago(ago: Duration) -> Self {
        LoopHeartbeat {
            origin: Instant::now()
                .checked_sub(ago)
                .expect("the monotonic clock reaches that far back"),
            last_us: AtomicU64::new(0),
        }
    }

    /// The loop completed an iteration.
    pub fn beat(&self) {
        let now_us = self.origin.elapsed().as_micros() as u64;
        self.last_us.store(now_us, Ordering::Relaxed);
    }

    /// How long since the last beat.
    pub fn age(&self) -> Duration {
        self.origin
            .elapsed()
            .saturating_sub(Duration::from_micros(self.last_us.load(Ordering::Relaxed)))
    }
}

impl Default for LoopHeartbeat {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the sync loop until `running` clears. `time_server` (the UDP 31900 time queries) answers
/// from `status` on its own thread, started here (after `main` raised the process to real-time
/// priority, which the thread inherits like the loop), only while the loop beats its
/// [`LoopHeartbeat`] (after every iteration), and stopped and joined when `run` ends — when
/// `running` clears (within one iteration and `TIME_SERVER_READ_TIMEOUT`) and when an iteration
/// panics ([`ServerGuard`]).
pub fn run<F: FnMut()>(
    time_server: Option<TimeServer>,
    status: Arc<RwLock<SyncStatus>>,
    running: Arc<AtomicBool>,
    mut iterate: F,
) {
    let heartbeat = Arc::new(LoopHeartbeat::new());
    let serving = Arc::new(AtomicBool::new(true));
    let _server = ServerGuard {
        thread: time_server.and_then(|ts| {
            match ts.spawn(status, heartbeat.clone(), serving.clone()) {
                Ok(handle) => Some(handle),
                Err(e) => {
                    warn!(
                        "[TimeServer] Could not start its thread: {} (continuing without it)",
                        e
                    );
                    None
                }
            }
        }),
        serving,
    };

    while running.load(Ordering::SeqCst) {
        iterate();
        heartbeat.beat();
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// dantesync#136 review — the time server lives exactly as long as `run`: dropped when `run`
/// returns or unwinds (a panicking iteration), it stops the server's thread and joins it. A dead
/// loop never leaves its server answering from the last status snapshot while the clock
/// free-runs (on Windows the loop runs on the service's own thread, so the process stays up); as
/// before 1.20, a dead loop answers nobody.
struct ServerGuard {
    /// The server's run flag (its own, not the daemon's `running`).
    serving: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        if std::thread::panicking() {
            error!("[TimeServer] the sync loop panicked: the time server stops answering");
        }
        self.serving.store(false, Ordering::SeqCst);
        if thread.join().is_err() {
            error!("[TimeServer] its thread ended with a panic");
        }
    }
}

#[cfg(test)]
mod tests;
