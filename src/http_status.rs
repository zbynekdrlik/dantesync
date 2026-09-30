//! HTTP status endpoint — serves the SAME status JSON the named pipe emits, over a
//! plain HTTP GET, so LAN automation (e.g. camera-box's CI runner) can read PTP/NTP
//! lock status without a human or an SMB/named-pipe bridge (dantesync#47).
//!
//! Deliberately hand-rolled over `std::net::TcpListener` — NOT tokio/hyper/warp.
//! `tokio` is currently a Windows-only Cargo dependency here (used only for the
//! named-pipe IPC server); pulling it into the Linux build just to serve one GET
//! route on a low-traffic monitoring port would balloon the dependency tree for no
//! functional gain. One blocking-accept thread + one short-lived thread per
//! connection is plenty for a handful of requests/minute from CI — the same
//! philosophy `ntp_server.rs` already uses for its UDP server.
//!
//! dantesync#126 — one ACTION route beside the status: `POST /date/step` from LOOPBACK only asks
//! the NTP master for one coordinated date step (`crate::date_step_trigger`). Every other request
//! still gets the status JSON, byte for byte as before.

use crate::date_step_trigger::{
    self, DateStepOutcome, DateStepRequest, Route, DATE_STEP_REPLY_TIMEOUT,
};
use crate::status::SyncStatus;
use log::{error, info, warn};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

/// dantesync#126 — where a `POST /date/step` goes: the sync loop's end of
/// `crate::date_step_trigger::channel` (`None`: the route answers that no loop takes it).
pub type DateStepSender = Option<mpsc::Sender<DateStepRequest>>;

/// Hard cap on concurrent in-flight connections. This is a read-only monitoring
/// endpoint on a trusted LAN, not a public service — the cap exists purely to bound
/// worst-case thread/memory usage if something opens many connections at once (a
/// port scanner, a misbehaving client), not to defend against a serious adversary.
const MAX_CONCURRENT_CONNECTIONS: usize = 32;

/// Start the HTTP status server in a background thread, bound to `0.0.0.0:port` so
/// it's reachable from OTHER machines on the LAN (not just localhost) — that's the
/// whole point: automation on a different host (e.g. the CI runner) reads status
/// without a human or an SMB/pipe bridge in between.
///
/// A bind failure (port in use, no permission) is logged and the endpoint is simply
/// disabled for this run — it must never take down the sync daemon itself.
pub fn start_http_status_server(
    status: Arc<RwLock<SyncStatus>>,
    port: u16,
    date_step: DateStepSender,
) {
    let bind_addr = format!("0.0.0.0:{}", port);
    match TcpListener::bind(&bind_addr) {
        Ok(listener) => {
            info!("[HTTP-Status] Listening on {}", bind_addr);
            spawn_accept_loop(listener, status, date_step);
        }
        Err(e) => {
            error!(
                "[HTTP-Status] Failed to bind {}: {} — endpoint disabled for this run",
                bind_addr, e
            );
        }
    }
}

/// RAII guard releasing one in-flight-connection slot when dropped — INCLUDING
/// during a panic unwind (this crate does not set `panic = "abort"`, so `Drop`
/// still runs while unwinding). Without this, a plain `counter.fetch_sub(...)`
/// placed as the last statement after `handle_connection(...)` would be skipped if
/// `handle_connection` ever panicked (no panic path exists there today, but nothing
/// stops one being introduced later, e.g. a `.unwrap()` added during maintenance) —
/// permanently leaking that slot and, after enough panics, wedging the connection
/// cap shut forever. Constructing the guard BEFORE calling `handle_connection`
/// closes that gap.
struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Testable seam: given an already-bound listener, spawn the accept loop. Production
/// code goes through `start_http_status_server` (binds `0.0.0.0:port`); tests bind an
/// ephemeral `127.0.0.1:0` listener directly so they need no fixed port and no LAN
/// exposure.
///
/// Hardenings on top of the naive "one thread per connection" (#47 review):
/// - **Connection cap** (`MAX_CONCURRENT_CONNECTIONS`): a connection accepted while
///   already at the cap is closed immediately, never spawned — bounds worst-case
///   thread/memory usage under a connection flood (port scanner, misbehaving
///   client) instead of growing unboundedly.
/// - **Non-panicking spawn**: bare `thread::spawn` PANICS if the OS can't create a
///   thread. That panic would unwind the SINGLE accept-loop thread it runs in,
///   permanently disabling the endpoint for the rest of the process's life. Using
///   `thread::Builder::spawn` (which returns `Result`) lets a spawn failure just log
///   and drop that one connection — the accept loop itself keeps running.
/// - **Panic-safe slot release** (`InFlightGuard`): the in-flight slot releases via
///   `Drop`, so a future panic inside `handle_connection` can't leak it.
fn spawn_accept_loop(
    listener: TcpListener,
    status: Arc<RwLock<SyncStatus>>,
    date_step: DateStepSender,
) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(conn) => {
                    // fetch_add returns the count BEFORE this connection — if it was
                    // already at the cap, this connection would push it over.
                    if in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_CONCURRENT_CONNECTIONS {
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        warn!(
                            "[HTTP-Status] At the {}-connection cap — dropping a new connection",
                            MAX_CONCURRENT_CONNECTIONS
                        );
                        continue; // `conn` drops here, closing the socket
                    }

                    let status = status.clone();
                    let date_step = date_step.clone();
                    let counter = in_flight.clone();
                    let spawned = thread::Builder::new()
                        .name("http-status-conn".into())
                        .spawn(move || {
                            // Constructed BEFORE handle_connection so its Drop
                            // releases the slot even if handle_connection panics.
                            let _guard = InFlightGuard { counter };
                            handle_connection(conn, &status, date_step.as_ref());
                        });
                    if let Err(e) = spawned {
                        // The thread never started, so no InFlightGuard was ever
                        // constructed to release the slot — do it here, or the cap
                        // would ratchet down permanently on every failed spawn.
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        warn!(
                            "[HTTP-Status] Failed to spawn connection handler thread: {} — dropping connection",
                            e
                        );
                    }
                }
                Err(e) => {
                    warn!("[HTTP-Status] Accept error: {}", e);
                }
            }
        }
    });
}

/// Handle a single connection. Read whatever the client sent, with a short read timeout so
/// a client that never sends anything can't wedge this thread forever. `POST /date/step`
/// (dantesync#126) goes to [`handle_date_step`]; EVERY other request is answered with the
/// current status JSON, as before — the SAME bytes the named pipe sends
/// (`SyncStatus::to_json_bytes`), so the two transports can never drift apart.
fn handle_connection(
    mut stream: TcpStream,
    status: &Arc<RwLock<SyncStatus>>,
    date_step: Option<&mpsc::Sender<DateStepRequest>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);

    match date_step_trigger::route(&buf[..n]) {
        Route::Status => {}
        Route::DateStepWrongMethod => {
            let body = date_step_trigger::refusal_body("use POST /date/step");
            write_response(&mut stream, 405, body.as_bytes());
            return;
        }
        Route::DateStep => {
            let (code, body) = handle_date_step(&stream, date_step);
            write_response(&mut stream, code, body.as_bytes());
            return;
        }
    }

    let body = match status.read() {
        Ok(guard) => match guard.to_json_bytes() {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("[HTTP-Status] Failed to serialize status: {}", e);
                write_response(&mut stream, 500, b"{}");
                return;
            }
        },
        Err(e) => {
            error!("[HTTP-Status] Status lock poisoned: {}", e);
            write_response(&mut stream, 500, b"{}");
            return;
        }
    };

    write_response(&mut stream, 200, &body);
}

/// dantesync#126 — `POST /date/step`: refused unless the peer is loopback (403); otherwise the
/// request goes to the sync loop, whose answer is `202` (one coordinated step announced) or
/// `409` (refused, with the reason), or `503` when no loop answers in time.
fn handle_date_step(
    stream: &TcpStream,
    date_step: Option<&mpsc::Sender<DateStepRequest>>,
) -> (u16, String) {
    let peer = stream.peer_addr().ok().map(|a| a.ip());
    if !peer.is_some_and(date_step_trigger::peer_allowed) {
        warn!(
            "[HTTP-Status] POST /date/step from {:?} refused: loopback only",
            peer
        );
        return (
            403,
            date_step_trigger::refusal_body("a date step is taken from loopback only"),
        );
    }
    let Some(sender) = date_step else {
        return (
            503,
            date_step_trigger::refusal_body("no sync loop takes date-step requests here"),
        );
    };
    let (reply, answer) = mpsc::channel();
    if sender.send(DateStepRequest { reply }).is_err() {
        return (
            503,
            date_step_trigger::refusal_body("the sync loop is not running"),
        );
    }
    match answer.recv_timeout(DATE_STEP_REPLY_TIMEOUT) {
        Ok(outcome) => {
            if let DateStepOutcome::Refused { reason } = &outcome {
                info!("[HTTP-Status] POST /date/step refused: {}", reason);
            }
            date_step_trigger::outcome_response(&outcome)
        }
        Err(_) => (
            503,
            date_step_trigger::refusal_body("the sync loop did not answer in time"),
        ),
    }
}

fn write_response(stream: &mut TcpStream, code: u16, body: &[u8]) {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        code,
        reason,
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deep-review finding (post-#48-review hardening): `InFlightGuard` exists
    /// specifically so a panic inside `handle_connection` can't permanently leak a
    /// connection-cap slot. Prove the mechanism directly — construct the guard,
    /// panic while it's alive, and confirm the counter still released.
    #[test]
    fn test_in_flight_guard_releases_slot_even_on_panic() {
        let counter = Arc::new(AtomicUsize::new(1)); // simulate one in-flight slot
        let counter_for_closure = counter.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = InFlightGuard {
                counter: counter_for_closure,
            };
            panic!("simulated handler panic while a connection slot is held");
        }));

        assert!(result.is_err(), "the panic should have propagated");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "InFlightGuard must release its slot even when the guarded code panics"
        );
    }

    fn locked_status() -> Arc<RwLock<SyncStatus>> {
        let status = SyncStatus {
            is_locked: true,
            mode: "LOCK".to_string(),
            offset_ns: 1234,
            ntp_offset_us: -150,
            ..Default::default()
        };
        Arc::new(RwLock::new(status))
    }

    /// #47: the HTTP endpoint must serve BYTE-IDENTICAL JSON to what the named pipe
    /// emits — i.e. `SyncStatus::to_json_bytes()`, the one shared serialization. This
    /// is the RED test: at this point `handle_connection` always answers `{}`, so it
    /// fails on the body comparison (not on connectivity — the request/response
    /// plumbing itself already works).
    #[test]
    fn test_http_status_endpoint_serves_same_json_as_pipe_payload() {
        let status = locked_status();
        let expected = status
            .read()
            .expect("read status")
            .to_json_bytes()
            .expect("serialize status");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        spawn_accept_loop(listener, status, None);

        let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect to status endpoint");
        conn.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        conn.write_all(b"GET /status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("write request");

        let mut response = Vec::new();
        conn.read_to_end(&mut response).expect("read response");
        let response = String::from_utf8(response).expect("response is valid utf8");

        let (headers, body) = response
            .split_once("\r\n\r\n")
            .expect("well-formed HTTP response (headers + body)");
        assert!(
            headers.starts_with("HTTP/1.1 200"),
            "expected 200 OK, got headers: {}",
            headers
        );
        assert!(
            headers
                .to_lowercase()
                .contains("content-type: application/json"),
            "expected JSON content-type, got headers: {}",
            headers
        );

        // The body must be byte-identical to the SAME serialization the named pipe
        // uses (SyncStatus::to_json_bytes) — no separate ad-hoc JSON building for
        // HTTP (the "one implementation, two consumers" requirement from #47).
        assert_eq!(
            body.as_bytes(),
            expected.as_slice(),
            "HTTP status body must match the pipe's SyncStatus::to_json_bytes() payload exactly"
        );
    }

    #[test]
    fn test_start_http_status_server_bind_failure_does_not_panic() {
        // Occupy a port, then try to start the server on the SAME port — bind must
        // fail gracefully (logged, no server thread), never panic the caller.
        let blocker = TcpListener::bind("127.0.0.1:0").expect("bind blocker");
        let port = blocker.local_addr().expect("local_addr").port();

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        // Bind on the SAME port via 127.0.0.1 explicitly is not what production code
        // does (it binds 0.0.0.0), but occupying the port on 0.0.0.0 across all
        // interfaces is the scenario we care about proving doesn't panic — reuse the
        // already-bound `blocker` port number against start_http_status_server.
        start_http_status_server(status, port, None);
        // No assertion beyond "did not panic" — a bind failure is logged and the
        // function returns normally.
        drop(blocker);
    }

    /// #47 review finding: `spawn_accept_loop` used to spawn ONE unbounded OS thread
    /// per accepted connection with no cap. Under a connection flood (port scanner,
    /// misbehaving client), that risks exhausting the process's thread limit — and
    /// since the un-capped code used a bare `thread::spawn` (which PANICS if the OS
    /// can't create a thread), the panic would unwind and kill the single accept-loop
    /// thread, permanently disabling the endpoint for the rest of the process's life.
    ///
    /// RED: proves the cap actually rejects a connection beyond
    /// `MAX_CONCURRENT_CONNECTIONS` (closes it immediately, no response written)
    /// rather than queuing/serving it — this fails against the un-capped code because
    /// every connection gets served.
    #[test]
    fn test_connection_cap_rejects_connections_beyond_the_limit() {
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        spawn_accept_loop(listener, status, None);

        // Hold MAX_CONCURRENT_CONNECTIONS connections open. Each server-side handler
        // thread blocks on its 2s read timeout (we never write anything), so the
        // in-flight count stays elevated for the duration of this test.
        let mut held: Vec<TcpStream> = Vec::new();
        for i in 0..MAX_CONCURRENT_CONNECTIONS {
            let conn = TcpStream::connect(("127.0.0.1", port))
                .unwrap_or_else(|e| panic!("connect #{} under the cap failed: {}", i, e));
            held.push(conn);
        }
        // Give the single accept-loop thread time to actually accept() each of the
        // above (TCP connect() completes via the kernel backlog before accept() does)
        // and bump the in-flight counter for all of them.
        thread::sleep(Duration::from_millis(300));

        // One more, over the cap, must be rejected: the accept loop closes it
        // immediately without ever spawning a handler or writing a response.
        let mut over_cap = TcpStream::connect(("127.0.0.1", port)).expect("connect over the cap");
        over_cap
            .set_read_timeout(Some(Duration::from_millis(800)))
            .expect("set read timeout");
        let mut buf = [0u8; 16];
        match over_cap.read(&mut buf) {
            Ok(0) => {} // closed cleanly with no bytes — rejected, exactly as expected
            other => panic!(
                "expected the over-cap connection to be closed with no response, got {:?}",
                other
            ),
        }

        drop(held);
    }

    /// dantesync#126 — send one raw request to a test server, return (headers, body).
    fn exchange(port: u16, raw: &[u8]) -> (String, String) {
        let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");
        conn.write_all(raw).expect("write request");
        let mut response = Vec::new();
        conn.read_to_end(&mut response).expect("read response");
        let response = String::from_utf8(response).expect("utf8");
        let (headers, body) = response.split_once("\r\n\r\n").expect("headers + body");
        (headers.to_string(), body.to_string())
    }

    /// A test server whose "sync loop" answers every date-step request with `outcome` (`None`:
    /// it takes the request and drops the reply unanswered).
    fn server_with_loop(outcome: Option<DateStepOutcome>) -> u16 {
        let (tx, rx) = date_step_trigger::channel();
        thread::spawn(move || {
            for req in rx {
                if let Some(o) = outcome.clone() {
                    let _ = req.reply.send(o);
                }
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        spawn_accept_loop(listener, locked_status(), Some(tx));
        port
    }

    const POST_STEP: &[u8] = b"POST /date/step HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

    #[test]
    fn a_loopback_post_date_step_reaches_the_sync_loop_and_answers_202_126() {
        let port = server_with_loop(Some(DateStepOutcome::Accepted {
            amount_ns: 247_297_000,
            land_ptp_ns: 42_000_000_000,
            due_in_ms: 10_000,
            seq: 6,
        }));
        let (headers, body) = exchange(port, POST_STEP);
        assert!(headers.starts_with("HTTP/1.1 202 Accepted"), "{headers}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(v["accepted"], true, "{body}");
        assert_eq!(v["amount_ns"], 247_297_000, "{body}");
        assert_eq!(v["land_ptp_ns"], 42_000_000_000_i64, "{body}");
        assert_eq!(v["seq"], 6, "{body}");
    }

    #[test]
    fn a_refused_date_step_is_409_with_the_reason_and_never_accepted_126() {
        let port = server_with_loop(Some(DateStepOutcome::Refused {
            reason: "the fleet is not behind UTC".to_string(),
        }));
        let (headers, body) = exchange(port, POST_STEP);
        assert!(headers.starts_with("HTTP/1.1 409 Conflict"), "{headers}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("JSON body");
        assert_eq!(v["accepted"], false, "{body}");
        assert_eq!(v["reason"], "the fleet is not behind UTC", "{body}");
    }

    #[test]
    fn a_sync_loop_that_does_not_answer_is_503_never_accepted_126() {
        let port = server_with_loop(None);
        let (headers, body) = exchange(port, POST_STEP);
        assert!(
            headers.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{headers}"
        );
        assert!(body.contains(r#""accepted":false"#), "{body}");
        // No loop wired at all: 503 too.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        spawn_accept_loop(listener, locked_status(), None);
        let (headers, _) = exchange(port, POST_STEP);
        assert!(headers.starts_with("HTTP/1.1 503"), "{headers}");
    }

    #[test]
    fn the_step_route_wants_post_and_every_other_request_still_gets_the_status_126() {
        let status = locked_status();
        let expected = status
            .read()
            .expect("read status")
            .to_json_bytes()
            .expect("serialize status");
        // A loop that would accept: a GET must never reach it.
        let port = server_with_loop(Some(DateStepOutcome::Accepted {
            amount_ns: 1,
            land_ptp_ns: 1,
            due_in_ms: 1,
            seq: 1,
        }));
        let (headers, body) = exchange(
            port,
            b"GET /date/step HTTP/1.1\r\nConnection: close\r\n\r\n",
        );
        assert!(
            headers.starts_with("HTTP/1.1 405 Method Not Allowed"),
            "{headers}"
        );
        assert!(body.contains(r#""accepted":false"#), "{body}");
        for raw in [
            &b"GET /status HTTP/1.1\r\nConnection: close\r\n\r\n"[..],
            &b"POST /status HTTP/1.1\r\nConnection: close\r\n\r\n"[..],
            &b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n"[..],
        ] {
            let (headers, body) = exchange(port, raw);
            assert!(headers.starts_with("HTTP/1.1 200 OK"), "{headers}");
            assert_eq!(body.as_bytes(), expected.as_slice(), "the status JSON");
        }
    }
}
