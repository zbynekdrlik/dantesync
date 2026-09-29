//! dantesync#112 — when to re-join the PTP multicast group, and how live the PTP receive is.
//!
//! A running node could keep `mode=LOCK` for days with no PTP frame on the wire. A USB NIC that is
//! re-plugged comes back under the same name but as a new netdev, and the socket's multicast
//! membership died with the old one. A Windows NIC swap leaves a dead pcap handle. Nothing ever
//! joined again: only a service restart recovered, and on the fleet's NTP master a restart
//! re-derives the fleet date offset (every box steps).
//!
//! The controller already knows when PTP is gone. No allowed PTP packet (a Sync or a Follow_Up
//! from a source the `gm_allowlist` allows) for `PTP_TIMEOUT_SECS` (10 s) is the `ptp_stale` of
//! the clock alarm. This module decides, from that one signal, when
//! to re-join (`PtpNetwork::rejoin`):
//!
//! - the first attempt once the silence reaches [`REJOIN_AFTER`];
//! - then while it lasts, [`REJOIN_BACKOFF`] after the previous attempt (30, 60, 120, then every
//!   300 s);
//! - after the next allowed packet, from scratch.
//!
//! One mechanism covers every cause: a dead membership, a dead capture handle, a lost IGMP
//! snooping entry, a grandmaster that went quiet. An interface-event watcher would see only the
//! first two.
//!
//! [`RxWindow`] measures the receive itself for `/status` (`last_ptp_rx_age_s`, `ptp_rx_pps`), so
//! a gate can assert liveness instead of trusting `is_locked`, and [`RejoinStatus`] is the
//! `/status.rejoin` object.
//!
//! Pure: explicit `Instant`s in, decisions out; no I/O, no logging.

use crate::traits::RejoinOutcome;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// The first re-join comes this long after the last allowed PTP packet: the moment the controller
/// calls PTP stale (`PTP_TIMEOUT_SECS`) and the clock alarm reports it.
pub const REJOIN_AFTER: Duration = Duration::from_secs(10);

/// The wait before each further attempt of one silence, measured from the previous attempt. The
/// last entry repeats for as long as the silence lasts.
pub const REJOIN_BACKOFF: [Duration; 4] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

/// The window of `/status.ptp_rx_pps`.
pub const RX_RATE_WINDOW: Duration = Duration::from_secs(10);

/// At most this many arrivals are kept (a grandmaster sends ~16 a second, so ~160 in a window): a
/// storm on 319/320 from an allowed source cannot grow the window without bound. Above it
/// `ptp_rx_pps` saturates at `RX_WINDOW_MAX_ARRIVALS / 10` (2000), which still reads as a storm.
pub const RX_WINDOW_MAX_ARRIVALS: usize = 20_000;

/// The wait before attempt `attempt` (1-based) of one silence: [`REJOIN_AFTER`] from the last
/// packet for the first, [`REJOIN_BACKOFF`] from the previous attempt for every later one.
pub fn rejoin_delay(attempt: u32) -> Duration {
    if attempt <= 1 {
        return REJOIN_AFTER;
    }
    let i = (attempt - 2) as usize;
    REJOIN_BACKOFF[i.min(REJOIN_BACKOFF.len() - 1)]
}

/// The re-join schedule of one silence.
#[derive(Clone, Debug, Default)]
pub struct RejoinSchedule {
    /// Attempts made in the current silence.
    attempts: u32,
    /// When the last of them was made.
    last_attempt: Option<Instant>,
}

impl RejoinSchedule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attempts made in the current silence.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Is a re-join due at `now`? `last_rx` is the last allowed PTP packet (or the start, when
    /// none has come yet).
    pub fn due(&self, last_rx: Instant, now: Instant) -> bool {
        match self.last_attempt {
            None => now.saturating_duration_since(last_rx) >= REJOIN_AFTER,
            Some(prev) => now.saturating_duration_since(prev) >= rejoin_delay(self.attempts + 1),
        }
    }

    /// An attempt was made at `now`, whether it worked or not. Returns its number within the
    /// silence.
    pub fn record_attempt(&mut self, now: Instant) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.last_attempt = Some(now);
        self.attempts
    }

    /// An allowed PTP packet arrived: the next silence starts from scratch. Returns the attempts
    /// the silence that just ended took.
    pub fn reset(&mut self) -> u32 {
        let ended = self.attempts;
        *self = Self::default();
        ended
    }
}

/// The allowed PTP packets of the last [`RX_RATE_WINDOW`], and when the newest one came.
#[derive(Clone, Debug, Default)]
pub struct RxWindow {
    /// Arrival instants inside the window, oldest first.
    arrivals: VecDeque<Instant>,
    last: Option<Instant>,
}

impl RxWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// One allowed packet arrived at `now` (arrivals come in time order). Arrivals older than
    /// the window are dropped here, and at most [`RX_WINDOW_MAX_ARRIVALS`] are kept, so the memory
    /// stays bounded even under a packet storm.
    pub fn record(&mut self, now: Instant) {
        self.last = Some(now);
        self.arrivals.push_back(now);
        while let Some(&oldest) = self.arrivals.front() {
            if now.saturating_duration_since(oldest) >= RX_RATE_WINDOW {
                self.arrivals.pop_front();
            } else {
                break;
            }
        }
        while self.arrivals.len() > RX_WINDOW_MAX_ARRIVALS {
            self.arrivals.pop_front();
        }
    }

    /// Allowed packets per second over the window ending at `now` (0 once the packets stop).
    pub fn pps(&self, now: Instant) -> f64 {
        let n = self
            .arrivals
            .iter()
            .filter(|&&t| now.saturating_duration_since(t) < RX_RATE_WINDOW)
            .count();
        n as f64 / RX_RATE_WINDOW.as_secs_f64()
    }

    /// Whole seconds since the newest allowed packet; `None` when none has come since the
    /// process started (never a misleading `0`).
    pub fn age_s(&self, now: Instant) -> Option<u64> {
        self.last
            .map(|t| now.saturating_duration_since(t).as_secs())
    }
}

/// `/status.rejoin`: the PTP re-joins this process made (dantesync#112). Additive; the default
/// (no attempt yet) is what an old JSON blob deserializes to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RejoinStatus {
    /// Re-join attempts since the process started, over every silence.
    pub attempts: u64,
    /// Unix epoch second of the last attempt; `null` before the first.
    pub last_ts: Option<u64>,
    /// The interface the last SUCCESSFUL re-join joined on; `null` before the first.
    pub last_iface: Option<String>,
    /// The IPv4 address the last successful re-join joined on.
    pub last_ip: Option<Ipv4Addr>,
    /// Why the last attempt failed; `null` when it worked (or none was made).
    pub last_error: Option<String>,
}

impl RejoinStatus {
    /// Record one attempt made at the wall second `epoch_s`.
    pub fn record(&mut self, epoch_s: u64, result: Result<&RejoinOutcome, &str>) {
        self.attempts = self.attempts.saturating_add(1);
        self.last_ts = Some(epoch_s);
        match result {
            Ok(outcome) => {
                self.last_iface = Some(outcome.iface.clone());
                self.last_ip = Some(outcome.ip);
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// Drive one silence second by second (a packet at `t0`, then nothing) and return the
    /// seconds, from `t0`, at which a re-join was made.
    fn attempt_seconds(len_s: u64) -> Vec<u64> {
        let t0 = Instant::now();
        let mut sched = RejoinSchedule::new();
        let mut made = Vec::new();
        for sec in 0..=len_s {
            let now = t0 + s(sec);
            if sched.due(t0, now) {
                sched.record_attempt(now);
                made.push(sec);
            }
        }
        made
    }

    #[test]
    fn the_delays_are_ten_then_thirty_sixty_one_twenty_then_three_hundred_for_good_112() {
        let got: Vec<u64> = (0..=9).map(|a| rejoin_delay(a).as_secs()).collect();
        assert_eq!(got, vec![10, 10, 30, 60, 120, 300, 300, 300, 300, 300]);
        assert_eq!(rejoin_delay(u32::MAX), s(300), "capped, never an overflow");
    }

    #[test]
    fn a_silence_is_re_joined_at_ten_seconds_then_on_the_backoff_112() {
        // 10, +30, +60, +120, then every 300 s.
        assert_eq!(
            attempt_seconds(1_200),
            vec![10, 40, 100, 220, 520, 820, 1_120]
        );
    }

    #[test]
    fn nothing_is_due_before_the_silence_reaches_ten_seconds_112() {
        let t0 = Instant::now();
        let sched = RejoinSchedule::new();
        assert!(!sched.due(t0, t0));
        assert!(!sched.due(t0, t0 + ms(9_999)));
        assert!(sched.due(t0, t0 + s(10)));
        // A packet stamped after `now` (never expected) is no silence at all.
        assert!(!sched.due(t0 + s(5), t0));
    }

    #[test]
    fn each_further_attempt_waits_from_the_previous_attempt_not_from_the_packet_112() {
        let t0 = Instant::now();
        let mut sched = RejoinSchedule::new();
        // The first attempt was late (the loop was busy): 13 s into the silence.
        assert_eq!(sched.record_attempt(t0 + s(13)), 1);
        assert!(!sched.due(t0, t0 + ms(42_999)));
        assert!(sched.due(t0, t0 + s(43)));
        assert_eq!(sched.record_attempt(t0 + s(43)), 2);
        assert_eq!(sched.attempts(), 2);
        assert!(!sched.due(t0, t0 + ms(102_999)));
        assert!(sched.due(t0, t0 + s(103)));
    }

    #[test]
    fn a_packet_restarts_the_schedule_from_scratch_112() {
        let t0 = Instant::now();
        let mut sched = RejoinSchedule::new();
        for at in [10, 40, 100] {
            assert!(sched.due(t0, t0 + s(at)));
            sched.record_attempt(t0 + s(at));
        }
        assert_eq!(sched.reset(), 3, "the ended silence took three attempts");
        assert_eq!(sched.attempts(), 0);
        // The packet came at 150 s; the next silence is re-joined 10 s after it, not after
        // the 120 s the old silence had reached.
        let packet = t0 + s(150);
        assert!(!sched.due(packet, packet + ms(9_999)));
        assert!(sched.due(packet, packet + s(10)));
        assert_eq!(sched.reset(), 0, "a reset with no attempt ends nothing");
    }

    #[test]
    fn the_rate_counts_the_last_ten_seconds_of_allowed_packets_112() {
        let t0 = Instant::now();
        let mut rx = RxWindow::new();
        assert_eq!(rx.age_s(t0), None, "no packet yet is not a fresh one");
        assert_eq!(rx.pps(t0), 0.0);
        // 16 packets a second (8 Sync + 8 Follow_Up) for 20 s.
        for i in 0..320u64 {
            rx.record(t0 + ms(i * 62_500 / 1_000));
        }
        let end = t0 + ms(19_938);
        assert!(
            (rx.pps(end) - 16.0).abs() < 0.2,
            "~16 pps, got {}",
            rx.pps(end)
        );
        assert_eq!(rx.age_s(end), Some(0));
        // The packets stop: the rate falls to 0 within the window and the age grows.
        assert!(rx.pps(end + s(5)) < 8.5);
        assert_eq!(rx.pps(end + s(10)), 0.0);
        assert_eq!(rx.age_s(end + ms(12_400)), Some(12));
    }

    #[test]
    fn the_rate_window_keeps_only_ten_seconds_of_arrivals_112() {
        let t0 = Instant::now();
        let mut rx = RxWindow::new();
        // 1 000 packets a second for a minute.
        for i in 0..60_000u64 {
            rx.record(t0 + ms(i));
        }
        assert!(
            rx.arrivals.len() <= 10_001,
            "bounded by the window, got {}",
            rx.arrivals.len()
        );
        assert!((rx.pps(t0 + ms(59_999)) - 1_000.0).abs() < 0.5);
    }

    #[test]
    fn a_packet_storm_cannot_grow_the_rate_window_without_bound_112() {
        let t0 = Instant::now();
        let mut rx = RxWindow::new();
        // 50 000 packets in half a second, all inside the window.
        for i in 0..50_000u64 {
            rx.record(t0 + Duration::from_micros(i * 10));
        }
        assert_eq!(rx.arrivals.len(), RX_WINDOW_MAX_ARRIVALS);
        assert_eq!(
            rx.pps(t0 + ms(500)),
            RX_WINDOW_MAX_ARRIVALS as f64 / 10.0,
            "saturated: still a storm"
        );
        assert_eq!(rx.age_s(t0 + ms(500)), Some(0));
    }

    #[test]
    fn the_status_keeps_the_last_success_and_the_last_error_112() {
        let ok = RejoinOutcome {
            iface: "enx002427159965".to_string(),
            ip: Ipv4Addr::new(10, 77, 9, 202),
            changed: false,
        };
        let mut st = RejoinStatus::default();
        st.record(1_790_000_000, Err("no suitable IPv4 interface"));
        assert_eq!(st.attempts, 1);
        assert_eq!(st.last_ts, Some(1_790_000_000));
        assert_eq!(st.last_error.as_deref(), Some("no suitable IPv4 interface"));
        assert_eq!(st.last_iface, None);
        st.record(1_790_000_030, Ok(&ok));
        assert_eq!(st.attempts, 2);
        assert_eq!(st.last_ts, Some(1_790_000_030));
        assert_eq!(st.last_error, None, "the last attempt worked");
        assert_eq!(st.last_iface.as_deref(), Some("enx002427159965"));
        assert_eq!(st.last_ip, Some(Ipv4Addr::new(10, 77, 9, 202)));
        st.record(1_790_000_090, Err("capture open failed"));
        assert_eq!(
            st.last_iface.as_deref(),
            Some("enx002427159965"),
            "a failed attempt keeps the last interface that worked"
        );
        assert_eq!(st.last_error.as_deref(), Some("capture open failed"));
    }
}
