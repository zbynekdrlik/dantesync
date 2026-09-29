//! dantesync#112 — the controller wiring of the PTP re-join and of the honest `/status` while PTP
//! is stale. The schedule itself is proven in `crate::ptp_rejoin`, the Linux socket re-create in
//! `crate::net_linux`, and the fleet date offset across a loss + re-acquire on the master in
//! `controller/date_sync/rejoin_tests.rs`.

use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, MockPtpNetwork, RejoinOutcome};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};

struct QuietNotifier;

impl ClockAlarmNotifier for QuietNotifier {
    fn notify(&self, _title: &str, _message: &str) {}
}

type TestController = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

const GM: Ipv4Addr = Ipv4Addr::new(10, 77, 9, 184);

/// A controller on `net` with a clock mock that has NO expectation: a clock or servo touch
/// anywhere in these tests panics it.
fn controller(net: MockPtpNetwork) -> (TestController, Arc<RwLock<SyncStatus>>) {
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    let mut c = PtpController::new(
        MockSystemClock::new(),
        net,
        MockNtpSource::new(),
        status.clone(),
        SystemConfig::default(),
    );
    // Never touch a real desktop bus from a unit test.
    c.clock_alarm_notifier = Box::new(QuietNotifier);
    (c, status)
}

fn eth0() -> RejoinOutcome {
    RejoinOutcome {
        iface: "enx002427159965".to_string(),
        ip: Ipv4Addr::new(10, 77, 9, 202),
        changed: false,
    }
}

/// A network whose re-joins succeed and are counted.
fn counting_network(count: Arc<AtomicU32>) -> MockPtpNetwork {
    let mut net = MockPtpNetwork::new();
    net.expect_rejoin().returning(move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(eth0())
    });
    net
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

/// A 60-byte PTPv1 datagram with the given control byte (0 Sync, 1 Delay_Req, 2 Follow_Up).
fn ptp_packet(control: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 60];
    buf[0] = 0x10; // PTPv1
    buf[32] = control;
    buf
}

/// The packets stopped `secs` ago. An `Instant` cannot be moved forward, so the receive history is
/// re-written instead: the last allowed packet came `secs` ago, and none after it (the loop only
/// ever records arrivals in time order).
fn went_quiet(c: &mut TestController, secs: u64) {
    let last = Instant::now() - Duration::from_secs(secs);
    c.ptp_liveness.rx = crate::ptp_rejoin::RxWindow::new();
    c.note_allowed_ptp_packet(last);
}

#[test]
fn packets_stop_then_one_rejoin_at_ten_seconds_then_the_backoff_112() {
    let count = Arc::new(AtomicU32::new(0));
    let (mut c, _st) = controller(counting_network(count.clone()));
    let t0 = Instant::now();
    c.note_allowed_ptp_packet(t0);
    let n = |c: &AtomicU32| c.load(Ordering::SeqCst);

    c.maybe_rejoin_ptp(t0 + ms(10_000));
    assert_eq!(n(&count), 0, "not stale at exactly 10 s");
    c.maybe_rejoin_ptp(t0 + ms(10_500));
    assert_eq!(n(&count), 1, "one re-join once PTP is stale");
    c.maybe_rejoin_ptp(t0 + ms(10_600));
    c.maybe_rejoin_ptp(t0 + ms(40_499));
    assert_eq!(n(&count), 1, "the next one waits 30 s");
    // 10.5, then +30, +60, +120, then every 300 s.
    for (at, want) in [
        (40_500, 2),
        (100_499, 2),
        (100_500, 3),
        (220_499, 3),
        (220_500, 4),
        (520_499, 4),
        (520_500, 5),
        (820_500, 6),
    ] {
        c.maybe_rejoin_ptp(t0 + ms(at));
        assert_eq!(n(&count), want, "at {at} ms");
    }
    assert_eq!(c.ptp_liveness.schedule.attempts(), 6);
}

#[test]
fn an_allowed_packet_restarts_the_schedule_112() {
    let count = Arc::new(AtomicU32::new(0));
    let (mut c, _st) = controller(counting_network(count.clone()));
    let t0 = Instant::now();
    c.note_allowed_ptp_packet(t0);
    for at in [10_500, 40_500, 100_500] {
        c.maybe_rejoin_ptp(t0 + ms(at));
    }
    assert_eq!(count.load(Ordering::SeqCst), 3);

    // The re-join worked: a packet at 150 s. The next silence is re-joined 10 s after it,
    // not after the 120 s the old one had reached.
    c.note_allowed_ptp_packet(t0 + ms(150_000));
    assert_eq!(c.ptp_liveness.schedule.attempts(), 0);
    c.maybe_rejoin_ptp(t0 + ms(159_000));
    assert_eq!(count.load(Ordering::SeqCst), 3);
    c.maybe_rejoin_ptp(t0 + ms(160_500));
    assert_eq!(count.load(Ordering::SeqCst), 4);
}

#[test]
fn the_loop_rejoins_before_it_receives_and_a_received_packet_resets_the_schedule_112() {
    let count = Arc::new(AtomicU32::new(0));
    let mut net = counting_network(count.clone());
    // A dead Npcap handle: every receive fails. The re-join must still come.
    net.expect_recv_packet().times(2).returning(|| {
        Err(anyhow::anyhow!(
            "Npcap recv error: PacketReceivePacket failed"
        ))
    });
    net.expect_recv_packet()
        .times(1)
        .returning(|| Ok(Some((ptp_packet(0), 60, SystemTime::now(), Some(GM)))));
    net.expect_recv_packet().returning(|| Ok(None));
    let (mut c, st) = controller(net);
    c.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);

    assert!(c.process_loop_iteration().is_err(), "the receive failed");
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "re-joined before the receive"
    );
    assert!(c.process_loop_iteration().is_err());
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "the backoff holds the next one"
    );
    {
        let st = st.read().expect("status");
        assert_eq!(
            st.rejoin.attempts, 1,
            "published at once, not at the next tick"
        );
        assert_eq!(st.rejoin.last_iface.as_deref(), Some("enx002427159965"));
        assert_eq!(st.rejoin.last_ip, Some(Ipv4Addr::new(10, 77, 9, 202)));
        assert!(st.rejoin.last_ts.is_some());
        assert_eq!(st.rejoin.last_error, None);
    }

    // The new handle delivers an allowed packet: the schedule starts from scratch and the
    // packet is counted.
    c.process_loop_iteration().expect("a packet");
    assert_eq!(c.ptp_liveness.schedule.attempts(), 0);
    c.update_shared_status();
    let st = st.read().expect("status");
    assert_eq!(st.last_ptp_rx_age_s, Some(0));
    assert!((st.ptp_rx_pps - 0.1).abs() < 1e-9, "1 packet / 10 s");
}

#[test]
fn a_failed_rejoin_is_published_and_retried_on_the_schedule_never_fatal_112() {
    let calls = Arc::new(AtomicU32::new(0));
    let seen = calls.clone();
    let mut net = MockPtpNetwork::new();
    net.expect_rejoin().returning(move || {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(anyhow::anyhow!("No suitable IPv4 interface found"))
        } else {
            Ok(eth0())
        }
    });
    let (mut c, st) = controller(net);
    let t0 = Instant::now();
    c.note_allowed_ptp_packet(t0);

    c.maybe_rejoin_ptp(t0 + ms(10_500));
    {
        let st = st.read().expect("status");
        assert_eq!(st.rejoin.attempts, 1);
        assert_eq!(
            st.rejoin.last_error.as_deref(),
            Some("No suitable IPv4 interface found")
        );
        assert_eq!(st.rejoin.last_iface, None);
    }
    c.maybe_rejoin_ptp(t0 + ms(40_000));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a failure keeps the backoff"
    );
    c.maybe_rejoin_ptp(t0 + ms(40_500));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let st = st.read().expect("status");
    assert_eq!(st.rejoin.attempts, 2);
    assert_eq!(st.rejoin.last_error, None);
    assert_eq!(st.rejoin.last_iface.as_deref(), Some("enx002427159965"));
}

#[test]
fn while_ptp_is_stale_status_is_not_locked_and_lock_returns_with_the_packets_112() {
    // The dev1 / imag / strih-lx case: locked, then no PTP packet — /status kept LOCK for days.
    let (mut c, st) = controller(MockPtpNetwork::new());
    c.is_locked = true;
    c.in_nano_mode = true;
    c.clock_settled = true;
    c.last_phase_offset_ns = 168_910_742;
    c.current_sync_source_ip = Some(GM);
    c.note_allowed_ptp_packet(Instant::now());
    c.tick_status();
    {
        let st = st.read().expect("status");
        assert!(st.is_locked);
        assert_eq!(st.mode, "NANO");
        assert!(st.settled);
    }

    // The grandmaster goes quiet: the offline edge, then the 10 s status tick (which used to
    // write LOCK straight back over the edge's "NTP-only").
    went_quiet(&mut c, PTP_TIMEOUT_SECS + 2);
    c.check_ptp_status();
    c.tick_status();
    {
        let st = st.read().expect("status");
        assert!(!st.is_locked, "no PTP packet for 12 s is not a lock");
        assert_eq!(st.mode, "NTP-only");
        assert!(!st.settled);
        assert_eq!(
            st.offset_ns, 168_910_742,
            "the last offset is kept, flagged by its age"
        );
        assert!(st.last_ptp_rx_age_s >= Some(PTP_TIMEOUT_SECS + 2));
        assert_eq!(st.ptp_rx_pps, 0.0);
        assert_eq!(st.gm_source_ip, Some(GM), "the last grandmaster heard");
        assert!(st.clock_alarm.active, "and the alarm says why");
    }

    // The packets come back: LOCK again, with no service restart.
    c.note_allowed_ptp_packet(Instant::now());
    c.check_ptp_status();
    c.tick_status();
    let st = st.read().expect("status");
    assert!(st.is_locked);
    assert_eq!(st.mode, "NANO");
    assert!(st.settled);
    assert_eq!(st.last_ptp_rx_age_s, Some(0));
}

#[test]
fn a_node_that_never_heard_ptp_reports_no_packet_age_112() {
    let (c, st) = controller(MockPtpNetwork::new());
    c.update_shared_status();
    let st = st.read().expect("status");
    assert_eq!(
        st.last_ptp_rx_age_s, None,
        "none since start, never a fresh 0"
    );
    assert_eq!(st.ptp_rx_pps, 0.0);
    assert_eq!(st.rejoin, crate::ptp_rejoin::RejoinStatus::default());
}

#[test]
fn the_receive_rate_counts_allowed_packets_only_112() {
    // 20 allowed packets over the last 10 s, and a foreign flood the allowlist drops.
    let mut net = MockPtpNetwork::new();
    net.expect_recv_packet().times(50).returning(|| {
        Ok(Some((
            vec![0x10],
            1,
            SystemTime::now(),
            Some(Ipv4Addr::new(10, 77, 9, 138)),
        )))
    });
    net.expect_recv_packet().returning(|| Ok(None));
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    let mut config = SystemConfig::default();
    config.gm_allowlist = vec!["10.77.9.184".to_string()];
    let mut c = PtpController::new(
        MockSystemClock::new(),
        net,
        MockNtpSource::new(),
        status.clone(),
        config,
    );
    c.clock_alarm_notifier = Box::new(QuietNotifier);
    let now = Instant::now();
    for i in 0..20u64 {
        c.note_allowed_ptp_packet(now - ms(9_500 - i * 500));
    }
    for _ in 0..50 {
        c.process_loop_iteration()
            .expect("a dropped packet is no error");
    }
    c.update_shared_status();
    let st = status.read().expect("status");
    assert!(
        (st.ptp_rx_pps - 2.0).abs() < 1e-9,
        "20 allowed packets / 10 s, the 50 foreign ones not counted: {}",
        st.ptp_rx_pps
    );
    assert_eq!(st.last_ptp_rx_age_s, Some(0));
}

#[test]
fn only_a_sync_or_follow_up_is_ptp_liveness_not_a_runt_or_a_delay_req_112() {
    // The grandmaster is gone, but other traffic from an allowed source still reaches 319/320
    // (an empty allowlist allows every source): a runt datagram and another follower's
    // Delay_Req. Neither is the grandmaster's time, so PTP stays stale and the re-join goes on.
    let count = Arc::new(AtomicU32::new(0));
    let mut net = counting_network(count.clone());
    net.expect_recv_packet()
        .times(1)
        .returning(|| Ok(Some((vec![0x10, 0x02], 2, SystemTime::now(), Some(GM)))));
    net.expect_recv_packet()
        .times(1)
        .returning(|| Ok(Some((ptp_packet(1), 60, SystemTime::now(), Some(GM)))));
    net.expect_recv_packet().returning(|| Ok(None));
    let (mut c, _st) = controller(net);
    let quiet_since = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);
    c.last_ptp_packet = quiet_since;

    c.process_loop_iteration().expect("a runt is no error");
    c.process_loop_iteration().expect("a Delay_Req is no error");
    assert_eq!(
        c.last_ptp_packet, quiet_since,
        "neither refreshed PTP liveness"
    );
    assert!(c.ptp_stale_at(Instant::now()));
    assert_eq!(
        c.ptp_liveness.rx.age_s(Instant::now()),
        None,
        "nothing counted"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "the re-join schedule runs on"
    );
}

#[test]
fn a_follow_up_from_the_grandmaster_is_ptp_liveness_112() {
    let mut net = MockPtpNetwork::new();
    net.expect_recv_packet()
        .times(1)
        .returning(|| Ok(Some((ptp_packet(2), 60, SystemTime::now(), Some(GM)))));
    net.expect_recv_packet().returning(|| Ok(None));
    let (mut c, _st) = controller(net);
    let before = Instant::now() - Duration::from_secs(5);
    c.last_ptp_packet = before;
    c.process_loop_iteration().expect("a Follow_Up");
    assert!(
        c.last_ptp_packet > before,
        "the Follow_Up refreshed PTP liveness"
    );
    assert_eq!(c.ptp_liveness.rx.age_s(Instant::now()), Some(0));
}
