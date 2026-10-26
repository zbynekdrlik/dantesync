//! dantesync#136 — the `ntp-burst` thread. Upper bounds only (no lower bound is measured across a
//! thread spawn); the slow upstream makes one burst last about a second.

use super::test_support::SlowNtpUpstream;
use super::*;
use crate::ntp::NtpClient;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One query of the slow upstream: a burst of 5 takes about a second.
const SLOW_ROUND_TRIP: Duration = Duration::from_millis(200);

/// A burst of the slow upstream is over well within this.
const BURST_BOUND: Duration = Duration::from_secs(10);

fn measurement(offset_us: u64) -> NtpMeasurement {
    NtpMeasurement {
        offset: Duration::from_micros(offset_us),
        sign: 1,
        spread_us: 7,
        sample_count: 3,
        pcap_active: false,
    }
}

/// Poll `take_result` (as the loop does, never blocking) until the burst is there.
fn wait_for_result(burst: &mut NtpBurstThread) -> Result<NtpMeasurement> {
    let started = Instant::now();
    loop {
        if let Some(result) = burst.take_result() {
            return result;
        }
        assert!(
            started.elapsed() < BURST_BOUND,
            "no burst result within {:?}",
            BURST_BOUND
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_real_burst_runs_on_its_own_thread_and_its_result_is_taken_once_136() {
    let upstream = SlowNtpUpstream::start(SLOW_ROUND_TRIP);
    let client = Arc::new(NtpClient::new(
        &upstream.server_name(),
        &crate::dscp::DscpConfig::default(),
    ));
    let running = Arc::new(AtomicBool::new(true));
    let measured = client.clone();
    let mut burst = NtpBurstThread::spawn(move || measured.get_offset(), running.clone())
        .expect("the ntp-burst thread");

    assert!(burst.start(), "the first burst starts");
    assert!(
        !burst.start(),
        "a second burst never starts while one is in flight"
    );
    assert!(
        burst.take_result().is_none(),
        "a burst of ~1 s is not over at once"
    );

    let m = wait_for_result(&mut burst).expect("the slow upstream answers every query");
    // The unchanged burst filter: the lowest-RTT 3 of the 5 samples.
    assert_eq!(m.sample_count, crate::ntp::NTP_BURST_ACCEPT_N);
    assert!(
        m.offset < Duration::from_millis(50),
        "a symmetric relay on the same clock measures ~0, got {}{:?}",
        m.sign,
        m.offset
    );
    assert!(burst.take_result().is_none(), "a result is taken once");

    // The next burst may start once the result was taken.
    assert!(burst.start(), "the next burst starts");
    wait_for_result(&mut burst).expect("the second burst");

    running.store(false, Ordering::SeqCst);
    burst.join().expect("the thread ends without a panic");
}

#[test]
fn a_failed_burst_is_delivered_as_its_error_136() {
    let running = Arc::new(AtomicBool::new(true));
    let mut burst = NtpBurstThread::spawn(
        || Err(anyhow::anyhow!("NTP burst: no server response")),
        running.clone(),
    )
    .expect("the ntp-burst thread");
    assert!(burst.start());
    let err = wait_for_result(&mut burst).expect_err("the error, not a measurement");
    assert!(err.to_string().contains("no server response"), "got: {err}");
    running.store(false, Ordering::SeqCst);
    burst.join().expect("the thread ends without a panic");
}

#[test]
fn a_burst_that_panics_is_a_failed_burst_and_the_thread_serves_the_next_one_136() {
    let running = Arc::new(AtomicBool::new(true));
    let mut calls = 0u32;
    let mut burst = NtpBurstThread::spawn(
        move || -> Result<NtpMeasurement> {
            calls += 1;
            if calls == 1 {
                panic!("a burst that panics (test)");
            }
            Ok(measurement(5))
        },
        running.clone(),
    )
    .expect("the ntp-burst thread");
    assert!(burst.start());
    let err = wait_for_result(&mut burst).expect_err("a panic is a failed burst");
    assert!(err.to_string().contains("panicked"), "got: {err}");
    assert!(burst.start(), "the thread is still there");
    let m = wait_for_result(&mut burst).expect("the next burst");
    assert_eq!(m.offset, Duration::from_micros(5));
    running.store(false, Ordering::SeqCst);
    burst.join().expect("the thread ends without a panic");
}

#[test]
fn a_burst_thread_that_has_ended_starts_nothing_136() {
    let running = Arc::new(AtomicBool::new(true));
    let burst = NtpBurstThread::spawn(|| Ok(measurement(1)), running.clone())
        .expect("the ntp-burst thread");
    running.store(false, Ordering::SeqCst);
    let mut burst = burst.joined().expect("the thread ended");
    assert!(
        !burst.start(),
        "nothing starts on an ended thread (the controller counts it as a failed check)"
    );
    assert!(burst.take_result().is_none());
}

#[test]
fn the_burst_thread_ends_within_a_bound_once_running_clears_even_mid_burst_136() {
    let running = Arc::new(AtomicBool::new(true));
    let mut burst = NtpBurstThread::spawn(
        || {
            std::thread::sleep(Duration::from_millis(300));
            Ok(measurement(42))
        },
        running.clone(),
    )
    .expect("the ntp-burst thread");
    assert!(burst.start(), "a burst in flight while running clears");
    running.store(false, Ordering::SeqCst);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(burst.join().is_ok());
    });
    let joined = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("the ntp-burst thread ended within 3 s of running clearing");
    assert!(joined, "and without a panic");
}

#[test]
fn an_idle_burst_thread_ends_within_a_bound_once_running_clears_136() {
    let running = Arc::new(AtomicBool::new(true));
    let burst = NtpBurstThread::spawn(|| Ok(measurement(1)), running.clone())
        .expect("the ntp-burst thread");
    running.store(false, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(burst.join().is_ok());
    });
    assert!(rx
        .recv_timeout(Duration::from_secs(2))
        .expect("an idle ntp-burst thread ended within 2 s of running clearing"));
}
