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

use crate::status::SyncStatus;
use log::{error, info, warn};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

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
pub fn start_http_status_server(status: Arc<RwLock<SyncStatus>>, port: u16) {
    let bind_addr = format!("0.0.0.0:{}", port);
    match TcpListener::bind(&bind_addr) {
        Ok(listener) => {
            info!("[HTTP-Status] Listening on {}", bind_addr);
            spawn_accept_loop(listener, status);
        }
        Err(e) => {
            error!(
                "[HTTP-Status] Failed to bind {}: {} — endpoint disabled for this run",
                bind_addr, e
            );
        }
    }
}

/// Testable seam: given an already-bound listener, spawn the accept loop. Production
/// code goes through `start_http_status_server` (binds `0.0.0.0:port`); tests bind an
/// ephemeral `127.0.0.1:0` listener directly so they need no fixed port and no LAN
/// exposure.
///
/// Two hardenings on top of the naive "one thread per connection" (#47 review):
/// - **Connection cap** (`MAX_CONCURRENT_CONNECTIONS`): a connection accepted while
///   already at the cap is closed immediately, never spawned — bounds worst-case
///   thread/memory usage under a connection flood (port scanner, misbehaving
///   client) instead of growing unboundedly.
/// - **Non-panicking spawn**: bare `thread::spawn` PANICS if the OS can't create a
///   thread. That panic would unwind the SINGLE accept-loop thread it runs in,
///   permanently disabling the endpoint for the rest of the process's life. Using
///   `thread::Builder::spawn` (which returns `Result`) lets a spawn failure just log
///   and drop that one connection — the accept loop itself keeps running.
fn spawn_accept_loop(listener: TcpListener, status: Arc<RwLock<SyncStatus>>) {
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
                    let counter = in_flight.clone();
                    let spawned = thread::Builder::new()
                        .name("http-status-conn".into())
                        .spawn(move || {
                            handle_connection(conn, &status);
                            counter.fetch_sub(1, Ordering::SeqCst);
                        });
                    if let Err(e) = spawned {
                        // The thread never started, so it will never decrement the
                        // counter itself — do it here, or the cap would ratchet down
                        // permanently on every failed spawn.
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

/// Handle a single connection. There is exactly one route (`GET /status`), so we
/// don't bother parsing the request line/headers beyond draining them — read
/// (and discard) whatever the client sent, with a short read timeout so a client
/// that never sends anything can't wedge this thread forever, then always respond
/// with the current status JSON — the SAME bytes the named pipe sends
/// (`SyncStatus::to_json_bytes`), so the two transports can never drift apart.
fn handle_connection(mut stream: TcpStream, status: &Arc<RwLock<SyncStatus>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);

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

fn write_response(stream: &mut TcpStream, code: u16, body: &[u8]) {
    let reason = if code == 200 {
        "OK"
    } else {
        "Internal Server Error"
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
        spawn_accept_loop(listener, status);

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
        start_http_status_server(status, port);
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
        spawn_accept_loop(listener, status);

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
}
