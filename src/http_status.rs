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
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

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
fn spawn_accept_loop(listener: TcpListener, status: Arc<RwLock<SyncStatus>>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(conn) => {
                    let status = status.clone();
                    thread::spawn(move || handle_connection(conn, &status));
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
/// with the current status JSON.
fn handle_connection(mut stream: TcpStream, status: &Arc<RwLock<SyncStatus>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);

    // TODO(#47): wire up the real status JSON — see the GREEN commit that follows
    // this one. This stub proves the RED test fails for the right reason (body
    // mismatch), not because the endpoint is unreachable.
    write_response(&mut stream, 200, b"{}");
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
        let mut status = SyncStatus::default();
        status.is_locked = true;
        status.mode = "LOCK".to_string();
        status.offset_ns = 1234;
        status.ntp_offset_us = -150;
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
}
