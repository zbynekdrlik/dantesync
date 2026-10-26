//! dantesync#136 — the time server answers while the sync loop is busy, and ends with the loop.
//! Real UDP on loopback (an ephemeral port: a running daemon may hold 31900). Upper bounds only.

use super::*;
use crate::time_server::{build_ext_request, parse_reply, STATUS_STALE_AFTER};
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long the loop stand-in holds one iteration (an NTP burst waiting on a loaded NIC, a long
/// PTP window): far longer than any reply may take.
const LOOP_HOLD: Duration = Duration::from_secs(2);

/// A time query is answered well within this (the follower's poller waits 500 ms, the camera-box
/// gate 200 ms; this bound is generous on purpose).
const REPLY_BOUND: Duration = Duration::from_secs(1);

/// The fleet's date master as the time server publishes it (the `"DSYX"` extension).
fn master_status() -> SyncStatus {
    SyncStatus {
        is_locked: true,
        mode: "LOCK".to_string(),
        gm_uuid: Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]),
        date_authority: "master".to_string(),
        date_offset_ns: Some(1_790_000_000_000_000_000),
        date_offset_effective_ptp_ns: Some(12_345_000_000_000),
        date_offset_seq: Some(3),
        date_offset_gm_uuid: Some([0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c]),
        ..SyncStatus::default()
    }
}

fn client() -> UdpSocket {
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client.set_read_timeout(Some(REPLY_BOUND)).unwrap();
    client
}

/// One `"DSYN"` query: the request id its reply echoes, `None` without a reply within `wait` (no
/// reply, or the port is closed).
fn ask(client: &UdpSocket, addr: SocketAddr, id: u32, wait: Duration) -> Option<u32> {
    client.set_read_timeout(Some(wait)).unwrap();
    let mut req = [0u8; 8];
    req[0..4].copy_from_slice(b"DSYN");
    req[4..8].copy_from_slice(&id.to_be_bytes());
    client.send_to(&req, addr).unwrap();
    let mut buf = [0u8; 256];
    match client.recv_from(&mut buf) {
        Ok((n, _)) if n >= 8 => Some(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])),
        _ => None,
    }
}

/// A loop that died ends its time server within this (one read timeout of the server's thread,
/// generous on purpose).
const SERVER_STOP_BOUND: Duration = Duration::from_secs(1);

#[test]
fn a_dsyx_query_is_answered_while_the_sync_loop_is_held_136() {
    let server = TimeServer::bind("127.0.0.1:0").expect("a time server on an ephemeral port");
    let addr = server.local_addr().unwrap();
    let status = Arc::new(RwLock::new(master_status()));
    let running = Arc::new(AtomicBool::new(true));

    // The loop stand-in: its first iteration holds for LOOP_HOLD (and says when it started).
    let (held_tx, held_rx) = mpsc::channel();
    let loop_running = running.clone();
    let sync_loop = thread::spawn(move || {
        let mut first = true;
        run(Some(server), status, loop_running, move || {
            if first {
                first = false;
                let _ = held_tx.send(());
                thread::sleep(LOOP_HOLD);
            }
        });
    });
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the loop started its held iteration");

    let client = client();
    client.send_to(&build_ext_request(9), addr).unwrap();
    let mut buf = [0u8; 256];
    let sent = Instant::now();
    let reply = client.recv_from(&mut buf);
    let waited = sent.elapsed();
    running.store(false, Ordering::SeqCst);
    sync_loop.join().expect("the loop ends");

    let (n, _) = reply.unwrap_or_else(|e| {
        panic!(
            "no DSYX reply within {:?} while the sync loop was held for {:?} ({}): the time \
             server answers only when the loop comes round",
            REPLY_BOUND, LOOP_HOLD, e
        )
    });
    assert!(waited < REPLY_BOUND, "answered after {:?}", waited);
    // The same answer as before: the base and the master's date extension.
    let parsed = parse_reply(&buf[..n], 9, Instant::now(), 0).expect("a DSYR reply to request 9");
    assert!(parsed.is_locked);
    let ext = parsed.ext.expect("the date-offset extension");
    assert!(ext.authority);
    assert_eq!(ext.announce.date_offset_ns, 1_790_000_000_000_000_000);
    assert_eq!(ext.announce.seq, 3);
}

#[test]
fn the_loop_ends_and_its_time_server_with_it_once_running_clears_136() {
    let server = TimeServer::bind("127.0.0.1:0").expect("a time server on an ephemeral port");
    let addr = server.local_addr().unwrap();
    let status = Arc::new(RwLock::new(master_status()));
    let running = Arc::new(AtomicBool::new(true));
    let (done_tx, done_rx) = mpsc::channel();
    let loop_running = running.clone();
    thread::spawn(move || {
        run(Some(server), status, loop_running, || {});
        let _ = done_tx.send(());
    });

    // It serves.
    let client = client();
    let mut req = [0u8; 8];
    req[0..4].copy_from_slice(b"DSYN");
    req[4..8].copy_from_slice(&1u32.to_be_bytes());
    client.send_to(&req, addr).unwrap();
    let mut buf = [0u8; 256];
    let (n, _) = client.recv_from(&mut buf).expect("a reply while running");
    assert_eq!(n, 64);

    running.store(false, Ordering::SeqCst);
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the loop returned within 2 s of running clearing");
    // The time server ended with it: its socket is closed, the port free again.
    UdpSocket::bind(addr).expect("the time server's port is free once the loop has returned");
}

/// Review of PR 137: a panicking iteration unwinds `run` — on Windows the loop runs on the
/// service's own thread, so the process stays up. Its time server must not go on answering from
/// the last status snapshot (locked, settled, the master's authority) while the clock free-runs:
/// in 1.19 a dead loop answered nobody, which followers and camera-box read as an outage.
#[test]
fn a_sync_loop_that_panics_takes_its_time_server_with_it_136() {
    let server = TimeServer::bind("127.0.0.1:0").expect("a time server on an ephemeral port");
    let addr = server.local_addr().unwrap();
    let status = Arc::new(RwLock::new(master_status()));
    let running = Arc::new(AtomicBool::new(true));
    let panic_now = Arc::new(AtomicBool::new(false));
    let loop_running = running.clone();
    let trigger = panic_now.clone();
    let sync_loop = thread::spawn(move || {
        run(Some(server), status, loop_running, move || {
            if trigger.load(Ordering::SeqCst) {
                panic!("a sync-loop iteration panics (test)");
            }
        });
    });

    let client = client();
    assert_eq!(
        ask(&client, addr, 1, REPLY_BOUND),
        Some(1),
        "it serves while the loop runs"
    );

    panic_now.store(true, Ordering::SeqCst);
    let died = Instant::now();
    while !sync_loop.is_finished() {
        assert!(
            died.elapsed() < Duration::from_secs(5),
            "the loop never ended after its panic"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let ended_after = died.elapsed();
    assert!(sync_loop.join().is_err(), "the loop ended by its panic");

    // `running` is still set: only the loop's death can have ended the server.
    let answered = ask(&client, addr, 2, Duration::from_millis(500));
    running.store(false, Ordering::SeqCst); // test hygiene: a server left behind ends now
    assert_eq!(
        answered, None,
        "the time server answered after the sync loop died (it answers from the last status \
         snapshot while the clock free-runs)"
    );
    assert!(
        ended_after < SERVER_STOP_BOUND,
        "the loop took {:?} to end with its time server",
        ended_after
    );
    UdpSocket::bind(addr).expect("the time server's port is free once the dead loop has returned");
}

/// Review of PR 137: a loop that is alive but has not completed an iteration for longer than
/// `STATUS_STALE_AFTER` (wedged) leaves the status snapshot stale: the server does not answer from
/// it, and answers again once the loop runs.
#[test]
fn a_wedged_sync_loop_s_time_server_waits_until_the_loop_runs_again_136() {
    let server = TimeServer::bind("127.0.0.1:0").expect("a time server on an ephemeral port");
    let addr = server.local_addr().unwrap();
    let status = Arc::new(RwLock::new(master_status()));
    let running = Arc::new(AtomicBool::new(true));
    let hold = STATUS_STALE_AFTER + Duration::from_secs(1);

    // The loop stand-in: its second iteration holds for `hold` (and says when it started).
    let (held_tx, held_rx) = mpsc::channel();
    let loop_running = running.clone();
    let sync_loop = thread::spawn(move || {
        let mut iteration = 0u32;
        run(Some(server), status, loop_running, move || {
            iteration += 1;
            if iteration == 2 {
                let _ = held_tx.send(());
                thread::sleep(hold);
            }
        });
    });
    held_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the loop started its held iteration");
    let held_at = Instant::now();
    let client = client();

    // At first the snapshot is fresh: answered while the iteration runs (#136).
    assert_eq!(ask(&client, addr, 1, REPLY_BOUND), Some(1));

    // Past the bound: not answered.
    thread::sleep(
        (held_at + STATUS_STALE_AFTER + Duration::from_millis(300))
            .saturating_duration_since(Instant::now()),
    );
    let stale = ask(&client, addr, 2, Duration::from_millis(400));
    // The loop runs again: answered again (and the query sent while it was stale got no reply).
    thread::sleep(
        (held_at + hold + Duration::from_millis(200)).saturating_duration_since(Instant::now()),
    );
    let fresh = ask(&client, addr, 3, REPLY_BOUND);
    running.store(false, Ordering::SeqCst);
    sync_loop.join().expect("the loop ends");

    assert_eq!(
        stale,
        None,
        "the time server answered while the sync loop had not completed an iteration for {:?} \
         (bound {:?})",
        Duration::from_millis(300) + STATUS_STALE_AFTER,
        STATUS_STALE_AFTER
    );
    assert_eq!(fresh, Some(3), "answered again once the loop runs");
}

#[test]
fn the_heartbeat_s_age_counts_from_its_last_beat_136() {
    assert!(LoopHeartbeat::new().age() < Duration::from_secs(1));
    let heartbeat = LoopHeartbeat::last_beat_ago(Duration::from_secs(10));
    assert!(heartbeat.age() >= Duration::from_secs(10));
    heartbeat.beat();
    assert!(heartbeat.age() < Duration::from_secs(1));
}
