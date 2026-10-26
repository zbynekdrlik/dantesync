//! dantesync#136 — the periodic NTP check with its burst off the sync loop: the loop keeps
//! iterating while a slow burst runs, and a burst's result does exactly what the inline check did.

use super::*;
use crate::clock::MockSystemClock;
use crate::ntp::{NtpClient, NtpMeasurement};
use crate::ntp_burst::test_support::SlowNtpUpstream;
use crate::ntp_burst::NtpBurstThread;
use crate::traits::{MockNtpSource, MockPtpNetwork, RejoinOutcome};
use mockall::predicate::eq;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

struct QuietNotifier;

impl ClockAlarmNotifier for QuietNotifier {
    fn notify(&self, _title: &str, _message: &str) {}
}

fn new_controller<S: NtpSource>(
    clock: MockSystemClock,
    net: MockPtpNetwork,
    ntp: S,
) -> (
    PtpController<MockSystemClock, MockPtpNetwork, S>,
    Arc<RwLock<SyncStatus>>,
) {
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    let mut c = PtpController::new(clock, net, ntp, status.clone(), SystemConfig::default());
    // Never touch a real desktop bus from a unit test.
    c.clock_alarm_notifier = Box::new(QuietNotifier);
    (c, status)
}

/// What the scripted runner was asked for and what it hands back.
#[derive(Default)]
struct Script {
    starts: u32,
    pending: Option<anyhow::Result<NtpMeasurement>>,
    refuse: bool,
}

/// A burst runner the test drives: it counts the starts and delivers what the test puts in.
#[derive(Clone, Default)]
struct ScriptedRunner(Rc<RefCell<Script>>);

impl ScriptedRunner {
    fn starts(&self) -> u32 {
        self.0.borrow().starts
    }

    fn deliver(&self, result: anyhow::Result<NtpMeasurement>) {
        self.0.borrow_mut().pending = Some(result);
    }
}

impl NtpBurstRunner for ScriptedRunner {
    fn start(&mut self) -> bool {
        let mut s = self.0.borrow_mut();
        if s.refuse {
            return false;
        }
        s.starts += 1;
        true
    }

    fn take_result(&mut self) -> Option<anyhow::Result<NtpMeasurement>> {
        self.0.borrow_mut().pending.take()
    }
}

fn reading(offset_us: u64, sign: i8) -> NtpMeasurement {
    NtpMeasurement {
        offset: Duration::from_micros(offset_us),
        sign,
        spread_us: 41,
        sample_count: 3,
        pcap_active: true,
    }
}

/// The scripted sequence both paths get: a +100 ms error seen twice (the #50 agreement gate steps
/// on the second), a small one, then three failed bursts (the third raises `ntp_failed`).
fn sequence(i: usize) -> anyhow::Result<NtpMeasurement> {
    match i {
        0 | 1 => Ok(reading(100_000, 1)),
        2 => Ok(reading(120, -1)),
        _ => Err(anyhow::anyhow!("NTP burst: no server response")),
    }
}

const SEQUENCE_LEN: usize = 6;

/// `/status` as JSON, its wall-clock stamps (seconds, taken at slightly different instants by the
/// two controllers) checked and then cleared.
fn comparable(
    status: &Arc<RwLock<SyncStatus>>,
    other: &Arc<RwLock<SyncStatus>>,
) -> (serde_json::Value, serde_json::Value) {
    let mut a = serde_json::to_value(status.read().unwrap().clone()).unwrap();
    let mut b = serde_json::to_value(other.read().unwrap().clone()).unwrap();
    for key in ["updated_ts", "ntp_updated_ts"] {
        let x = a[key].as_u64().unwrap_or(0);
        let y = b[key].as_u64().unwrap_or(0);
        assert!(x.abs_diff(y) <= 1, "{key}: {x} vs {y}");
        assert_eq!(x == 0, y == 0, "{key}: set on one side only ({x} vs {y})");
        a[key] = serde_json::Value::Null;
        b[key] = serde_json::Value::Null;
    }
    (a, b)
}

fn step_once(clock: &mut MockSystemClock) {
    clock
        .expect_step_clock()
        .with(eq(Duration::from_millis(100)), eq(1))
        .times(1)
        .returning(|_, _| Ok(()));
}

#[test]
fn a_burst_result_publishes_exactly_what_the_inline_check_did_136() {
    // The inline check (pre-1.20, and every controller without a runner).
    let mut inline_clock = MockSystemClock::new();
    step_once(&mut inline_clock);
    let mut calls = VecDeque::from_iter(0..SEQUENCE_LEN);
    let mut inline_ntp = MockNtpSource::new();
    inline_ntp
        .expect_get_offset()
        .times(SEQUENCE_LEN)
        .returning(move || sequence(calls.pop_front().unwrap()));
    let (mut inline, inline_status) =
        new_controller(inline_clock, MockPtpNetwork::new(), inline_ntp);
    inline.ptp_offline = true;

    // The same check with its burst on a runner: the inline source is never asked.
    let mut clock = MockSystemClock::new();
    step_once(&mut clock);
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(clock, MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;

    for i in 0..SEQUENCE_LEN {
        inline.last_ntp_check = Instant::now() - Duration::from_secs(60);
        inline.check_ntp_utc_tracking();

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        let before = serde_json::to_value(status.read().unwrap().clone()).unwrap();
        c.check_ntp_utc_tracking();
        assert_eq!(runner.starts(), i as u32 + 1, "check {i} started its burst");
        assert!(
            c.last_ntp_check.elapsed() < Duration::from_secs(5),
            "check {i}: the cadence counts from the request, as it did from the inline burst"
        );
        c.service_ntp_burst();
        assert_eq!(
            serde_json::to_value(status.read().unwrap().clone()).unwrap(),
            before,
            "check {i}: nothing is published while the burst runs"
        );

        runner.deliver(sequence(i));
        c.service_ntp_burst();
        let (want, got) = comparable(&inline_status, &status);
        assert_eq!(got, want, "check {i}: the same /status as the inline check");
        assert_eq!(
            c.ntp_consecutive_failures, inline.ntp_consecutive_failures,
            "check {i}"
        );
        assert_eq!(c.ntp_pending_step, inline.ntp_pending_step, "check {i}");
        assert_eq!(c.ntp_offset_samples, inline.ntp_offset_samples, "check {i}");
    }
    let s = status.read().unwrap();
    assert!(s.ntp_failed, "three failed bursts raise ntp_failed");
    assert!(!s.pcap_ntp_active);
}

#[test]
fn no_second_burst_starts_while_one_is_in_flight_136() {
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(MockSystemClock::new(), MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;

    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 1);

    // Due again (a burst that outlasts the interval): it waits for the one in flight.
    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 1, "no second burst while one is in flight");
    assert!(
        c.last_ntp_check.elapsed() >= Duration::from_secs(59),
        "the cadence is untouched while waiting"
    );

    runner.deliver(Ok(reading(250, 1)));
    c.service_ntp_burst();
    assert_eq!(status.read().unwrap().ntp_offset_us, 250);
    // Overdue, so the next one starts at once (as the inline loop ran the next burst right away).
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 2);
}

#[test]
fn a_burst_that_ran_across_a_wall_step_is_not_used_136() {
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .with(eq(Duration::from_secs(2)), eq(1))
        .times(1)
        .returning(|_, _| Ok(()));
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(clock, MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;

    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 1);
    // The wall is stepped while the burst runs (here a boot step; a date step counts the same).
    assert!(c.step_boot_offset(Duration::from_secs(2), 1));

    // Its samples straddle the step: it must not be taken for this wall's offset.
    runner.deliver(Ok(reading(100_000, 1)));
    c.service_ntp_burst();
    {
        let s = status.read().unwrap();
        assert_eq!(s.ntp_updated_ts, 0, "the straddling burst is not published");
        assert_eq!(s.ntp_offset_us, 0);
    }
    assert!(c.ntp_pending_step.is_none(), "nor counted toward a step");
    assert!(c.ntp_offset_samples.is_empty());

    // The next burst, on the stepped wall, is used.
    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 2);
    runner.deliver(Ok(reading(300, 1)));
    c.service_ntp_burst();
    assert_eq!(status.read().unwrap().ntp_offset_us, 300);
}

#[test]
fn a_burst_that_arrives_while_the_boot_reading_is_pending_gives_way_to_it_136() {
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(MockSystemClock::new(), MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;

    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 1);
    // A boot reading is measured again meanwhile (a deferred boot step, an LI=3 retry).
    let (tx, rx) = std::sync::mpsc::channel();
    c.start_boot_ntp(rx);

    runner.deliver(Ok(reading(100_000, 1)));
    c.service_ntp_burst();
    assert_eq!(
        status.read().unwrap().ntp_updated_ts,
        0,
        "the periodic burst gives way to the boot reading"
    );
    assert!(c.ntp_pending_step.is_none());

    tx.send(Ok(reading(3_000, 1))).unwrap();
    c.service_boot_ntp();
    assert_eq!(status.read().unwrap().ntp_offset_us, 3_000);
}

fn failed_burst() -> anyhow::Result<NtpMeasurement> {
    Err(anyhow::anyhow!("NTP burst: no server response"))
}

/// Review of PR 137: a FAILED burst has no samples to straddle a step — it counts, as 1.19's
/// inline one did. A timed-out burst lasts ~15 s; with an upstream that silently drops packets
/// while date micro-steps keep landing, dropping the failures would hold `ntp_failed` back (and
/// leave `pcap_ntp_active` set).
#[test]
fn a_failed_burst_that_ran_across_a_wall_step_still_counts_as_a_failure_136() {
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .with(eq(Duration::from_secs(2)), eq(1))
        .times(3)
        .returning(|_, _| Ok(()));
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(clock, MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;
    status.write().unwrap().pcap_ntp_active = true;

    for i in 0..3u32 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        assert_eq!(runner.starts(), i + 1, "check {i} started its burst");
        // The wall is stepped while the burst runs, and the burst fails.
        assert!(c.step_boot_offset(Duration::from_secs(2), 1));
        runner.deliver(failed_burst());
        c.service_ntp_burst();
        assert_eq!(
            c.ntp_consecutive_failures,
            i as usize + 1,
            "failed burst {i} across a wall step counts as a failed check"
        );
        assert!(
            !status.read().unwrap().pcap_ntp_active,
            "failed burst {i}: no transport measured anything"
        );
    }
    assert!(
        status.read().unwrap().ntp_failed,
        "three failed bursts raise ntp_failed, steps or not"
    );
}

/// Review of PR 137: the boot reading comes first for a MEASUREMENT; a failed burst is not one
/// and counts.
#[test]
fn a_failed_burst_counts_while_the_boot_reading_is_pending_136() {
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(MockSystemClock::new(), MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;
    status.write().unwrap().pcap_ntp_active = true;

    c.last_ntp_check = Instant::now() - Duration::from_secs(60);
    c.check_ntp_utc_tracking();
    assert_eq!(runner.starts(), 1);
    let (tx, rx) = std::sync::mpsc::channel();
    c.start_boot_ntp(rx);

    runner.deliver(failed_burst());
    c.service_ntp_burst();
    assert_eq!(
        c.ntp_consecutive_failures, 1,
        "the failed burst counts although the boot reading is pending"
    );
    assert!(!status.read().unwrap().pcap_ntp_active);

    // The boot reading still comes first and is applied as always.
    tx.send(Ok(reading(3_000, 1))).unwrap();
    c.service_boot_ntp();
    assert_eq!(status.read().unwrap().ntp_offset_us, 3_000);
    assert_eq!(c.ntp_consecutive_failures, 0);
}

#[test]
fn a_runner_that_cannot_start_a_burst_counts_as_a_failed_check_136() {
    let mut never = MockNtpSource::new();
    never.expect_get_offset().times(0);
    let (mut c, status) = new_controller(MockSystemClock::new(), MockPtpNetwork::new(), never);
    let runner = ScriptedRunner::default();
    runner.0.borrow_mut().refuse = true;
    c.set_ntp_burst_runner(Box::new(runner.clone()));
    c.ptp_offline = true;
    status.write().unwrap().pcap_ntp_active = true;

    for _ in 0..3 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
    }
    let s = status.read().unwrap();
    assert!(
        s.ntp_failed,
        "three checks without a burst raise ntp_failed"
    );
    assert!(!s.pcap_ntp_active);
    assert_eq!(c.ntp_consecutive_failures, 3);
}

/// One loop pass as `crate::sync_loop::run` makes it (the iteration, then the poll interval).
const SLOW_ROUND_TRIP: Duration = Duration::from_millis(200);

/// The loop's longest pause while the ~1 s burst runs (normally ~1 ms; generous on purpose).
const LOOP_GAP_BOUND: Duration = Duration::from_millis(300);

#[test]
fn the_loop_keeps_iterating_while_a_slow_burst_runs_on_its_thread_136() {
    let upstream = SlowNtpUpstream::start(SLOW_ROUND_TRIP);
    let client = Arc::new(NtpClient::new(
        &upstream.server_name(),
        &crate::dscp::DscpConfig::default(),
    ));
    let mut net = MockPtpNetwork::new();
    net.expect_recv_packet().returning(|| Ok(None));
    net.expect_rejoin().returning(|| {
        Ok(RejoinOutcome {
            iface: "eth0".to_string(),
            ip: Ipv4Addr::new(192, 0, 2, 10),
            changed: false,
        })
    });
    // The controller and its thread share one client, as in `main`.
    let (mut c, status) = new_controller(MockSystemClock::new(), net, client.clone());
    let running = Arc::new(AtomicBool::new(true));
    let measured = client.clone();
    let burst = NtpBurstThread::spawn(move || measured.get_offset(), running.clone())
        .expect("the ntp-burst thread");
    c.set_ntp_burst_runner(Box::new(burst));
    // PTP has been silent for a minute (NTP-only): the periodic check is due at once.
    c.last_ptp_packet = Instant::now() - Duration::from_secs(60);
    c.last_ntp_check = Instant::now() - Duration::from_secs(60);

    let started = Instant::now();
    let mut longest = Duration::ZERO;
    let mut last = Instant::now();
    let mut iterations = 0u32;
    while status.read().unwrap().ntp_updated_ts == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the burst's result never reached the controller"
        );
        c.process_loop_iteration().expect("an iteration");
        iterations += 1;
        std::thread::sleep(crate::sync_loop::POLL_INTERVAL);
        let now = Instant::now();
        longest = longest.max(now - last);
        last = now;
    }
    running.store(false, Ordering::SeqCst);

    assert!(
        longest < LOOP_GAP_BOUND,
        "the sync loop stalled {:?} while the NTP burst ran (bound {:?}, {} iterations)",
        longest,
        LOOP_GAP_BOUND,
        iterations
    );
    let s = status.read().unwrap();
    assert!(!s.ntp_failed);
    assert_eq!(s.ntp_sample_count, crate::ntp::NTP_BURST_ACCEPT_N);
    assert!(
        s.ntp_offset_us.abs() < 50_000,
        "a symmetric relay on the same clock measures ~0, got {} us",
        s.ntp_offset_us
    );
}
