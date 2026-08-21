//! Minimal NTP/SNTP Server implementation for DanteSync.
//!
//! This module provides a simple NTP server that responds to client queries
//! with the current system time. It's designed for use in Dante audio networks
//! where one machine serves as the time master, disciplined by PTP, and all
//! other machines sync their time via NTP from this master.
//!
//! The server implements RFC 5905 (NTPv4) at a basic level, supporting:
//! - NTPv3 and NTPv4 client requests
//! - Standard 48-byte NTP packet format
//! - Configurable stratum level
//!
//! This is NOT a full-featured NTP server. It's optimized for LAN use where
//! all clients trust this server as the authoritative time source.

use anyhow::{anyhow, Result};
use log::{debug, error, info, warn};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ============================================================================
// NTP PROTOCOL CONSTANTS
// ============================================================================

/// NTP packet size (48 bytes)
const NTP_PACKET_SIZE: usize = 48;

/// NTP epoch offset from Unix epoch (1900-01-01 to 1970-01-01 in seconds)
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

/// LI (Leap Indicator): 0 = no warning
const LI_NO_WARNING: u8 = 0;

/// Mode: 4 = server
const MODE_SERVER: u8 = 4;

/// Mode: 3 = client
const MODE_CLIENT: u8 = 3;

/// Reference ID for local clock (ASCII "LOCL")
const REF_ID_LOCL: u32 = 0x4C4F434C;

/// How `run()` should react to a `recv_from` error (#68).
///
/// A UDP receive loop must distinguish three very different things that all
/// arrive as `io::Error`, because treating them alike is what made a routine
/// condition degrade the fleet's time source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecvErrorAction {
    /// Nothing arrived within the read timeout — the normal idle path.
    Idle,
    /// An EXPECTED, harmless condition: a previously-sent datagram bounced
    /// (ICMP port-unreachable) and the OS reports it on the next receive.
    /// Windows calls this `WSAECONNRESET` (os error 10054) and raises it
    /// routinely for a server whose clients come and go. Continue immediately.
    Benign,
    /// Genuinely unexpected — log it and back off briefly so a hard-failing
    /// socket cannot spin the CPU.
    Backoff,
}

impl RecvErrorAction {
    /// How long to sleep before the next receive. `None` for everything that
    /// is normal: a stalled server answers nobody, and on the fleet's time
    /// source that is the actual harm a benign reset used to cause.
    fn backoff(self) -> Option<Duration> {
        match self {
            RecvErrorAction::Idle | RecvErrorAction::Benign => None,
            RecvErrorAction::Backoff => Some(RECV_ERROR_BACKOFF),
        }
    }
}

/// Backoff applied only to a genuinely unknown receive error.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// #68 — classify a `recv_from` error. Pure, so the policy is unit-tested
/// directly instead of being inferred from a live socket (WSAECONNRESET in
/// particular cannot be provoked at all on Linux, where an unconnected UDP
/// socket never surfaces the ICMP error).
fn classify_recv_error(kind: std::io::ErrorKind) -> RecvErrorAction {
    match kind {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => RecvErrorAction::Idle,
        // ConnectionReset  = WSAECONNRESET (Windows, the reported one).
        // ConnectionRefused = the POSIX ICMP-unreachable equivalent.
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused => {
            RecvErrorAction::Benign
        }
        _ => RecvErrorAction::Backoff,
    }
}

/// #68 — Windows raises `WSAECONNRESET` on a UDP socket's next `recv_from`
/// after one of its outbound datagrams drew an ICMP port-unreachable. That is
/// pointless for a *server* socket, and `SIO_UDP_CONNRESET = FALSE` is the
/// documented way to stop the stack reporting it at all (Microsoft's own
/// guidance for UDP servers; the classifier above is the belt to this braces).
///
/// Never fatal: a failure here only means the benign errors keep surfacing, and
/// `classify_recv_error` already handles them — so it is logged and ignored
/// rather than taking down the fleet's time source at startup.
#[cfg(windows)]
fn disable_udp_conn_reset(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows::Win32::Networking::WinSock::{WSAIoctl, SIO_UDP_CONNRESET, SOCKET};

    let raw = SOCKET(socket.as_raw_socket() as usize);
    let disable: u32 = 0; // FALSE
    let mut bytes_returned: u32 = 0;
    // SAFETY: `disable` and `bytes_returned` are stack locals that outlive this
    // synchronous call (`lpOverlapped` is NULL, so WSAIoctl cannot complete
    // later); the in-buffer is only READ by the ioctl and its declared size
    // matches `u32` (layout-identical to the documented BOOL); `SOCKET(..)` is a
    // non-owning handle wrapper, so nothing here can double-close the socket.
    let rc = unsafe {
        WSAIoctl(
            raw,
            SIO_UDP_CONNRESET,
            Some(&disable as *const u32 as *const std::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut bytes_returned,
            None,
            None,
        )
    };
    if rc != 0 {
        warn!(
            "[NTP-Server] Could not disable SIO_UDP_CONNRESET: {} (harmless — benign \
             resets are classified and ignored anyway)",
            std::io::Error::last_os_error()
        );
    } else {
        debug!("[NTP-Server] SIO_UDP_CONNRESET disabled (spurious UDP resets suppressed)");
    }
}

// ============================================================================
// NTP SERVER
// ============================================================================

/// Minimal NTP server that responds to client queries with current system time.
///
/// # Usage
/// ```ignore
/// let server = NtpServer::new(123, 3, Default::default())?;
/// server.run(running_flag)?;
/// ```
pub struct NtpServer {
    socket: UdpSocket,
    stratum: u8,
    /// The port this server was CONFIGURED with (#68) — kept so `run_supervised`
    /// can re-bind after an unexpected exit. Note the rebuild is not identical:
    /// the status source is re-attached explicitly by the supervisor, and a
    /// server configured with port 0 re-binds to a DIFFERENT ephemeral port
    /// (only reachable in tests; production always configures a real port).
    port: u16,
    /// Fallback reference timestamp: when this server was created. Used only
    /// until the first upstream measurement lands (#68).
    reference_time: SystemTime,
    /// #68: the live sync status, read for `ntp_updated_ts` — when this host
    /// last actually re-read its own upstream reference. `None` (or a status
    /// with nothing measured yet) falls back to `reference_time`.
    status: Option<Arc<std::sync::RwLock<crate::status::SyncStatus>>>,
    /// dantesync#52 — DSCP marking config, carried so `run_supervised`'s rebind
    /// re-applies the mark on a fresh socket after an unexpected exit.
    dscp: crate::dscp::DscpConfig,
}

impl NtpServer {
    /// Create a new NTP server bound to the specified port.
    ///
    /// # Arguments
    /// * `port` - UDP port to listen on (usually 123, requires elevated privileges)
    /// * `stratum` - Stratum level to report (typically 2-4 for LAN servers)
    pub fn new(port: u16, stratum: u8, dscp: crate::dscp::DscpConfig) -> Result<Self> {
        let bind_addr = format!("0.0.0.0:{}", port);
        let socket = UdpSocket::bind(&bind_addr).map_err(|e| {
            anyhow!(
                "Failed to bind NTP server to {}: {} (hint: port 123 requires root/admin)",
                bind_addr,
                e
            )
        })?;

        // #340: BLOCKING socket with a read timeout — NOT non-blocking. These two settings are
        // MUTUALLY EXCLUSIVE: with set_nonblocking(true), recv_from returns WouldBlock instantly and
        // the read timeout is ignored, so run()'s loop busy-spins a full CPU core (observed on strih,
        // the NTP master, ~99% of one core continuously). The 100ms read timeout alone gives graceful
        // shutdown (the loop re-checks `running` every <=100ms) AND ~0% idle CPU (recv_from sleeps in
        // the kernel between packets). Do NOT re-add set_nonblocking.
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;

        // #68: stop Windows raising WSAECONNRESET on this server socket at all.
        #[cfg(windows)]
        disable_udp_conn_reset(&socket);

        // dantesync#52: mark this reply socket's egress with DSCP so loaded venue
        // switches prioritise timesync. Linux-effective; a logged no-op on Windows
        // (IP_TOS is filtered there — see crate::dscp). Best-effort: a failure
        // leaves the socket unmarked, exactly the pre-#52 behaviour.
        crate::dscp::apply(&socket, &dscp, "ntp-server reply");

        info!(
            "[NTP-Server] Listening on {} (stratum {})",
            bind_addr, stratum
        );

        Ok(NtpServer {
            socket,
            stratum,
            port,
            reference_time: SystemTime::now(),
            status: None,
            dscp,
        })
    }

    /// #68 — attach the live sync status so the served reference timestamp
    /// tracks the last REAL upstream sync instead of process start.
    pub fn set_status_source(&mut self, status: Arc<std::sync::RwLock<crate::status::SyncStatus>>) {
        self.status = Some(status);
    }

    /// When this host last re-read its own upstream reference (#68), falling
    /// back to server start while nothing has been measured yet — never to the
    /// 1970 epoch a bare `ntp_updated_ts: 0` would produce.
    fn reference_timestamp(&self) -> SystemTime {
        if let Some(status) = &self.status {
            if let Ok(s) = status.read() {
                if s.ntp_updated_ts > 0 {
                    // `checked_add`, not `+`: `SystemTime: Add<Duration>` PANICS
                    // on overflow, and this runs on the server thread handling
                    // network requests. A nonsensical timestamp degrades to the
                    // fallback instead of killing the fleet's time source.
                    return UNIX_EPOCH
                        .checked_add(Duration::from_secs(s.ntp_updated_ts))
                        .unwrap_or(self.reference_time);
                }
            }
        }
        self.reference_time
    }

    /// The address this server is actually bound to (#68). Differs from the
    /// configured port when bound to port 0 — which is why this exists at all:
    /// it is the seam that lets a test drive a real ephemeral-port server.
    #[cfg(test)]
    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    /// Run the NTP server loop until the running flag is cleared.
    pub fn run(&self, running: Arc<AtomicBool>) -> Result<()> {
        let mut buf = [0u8; NTP_PACKET_SIZE];

        while running.load(Ordering::SeqCst) {
            match self.socket.recv_from(&mut buf) {
                Ok((size, src)) => {
                    if size >= NTP_PACKET_SIZE {
                        if let Err(e) = self.handle_request(&buf, src) {
                            warn!("[NTP-Server] Error handling request from {}: {}", src, e);
                        }
                    } else {
                        debug!(
                            "[NTP-Server] Ignoring short packet ({} bytes) from {}",
                            size, src
                        );
                    }
                }
                // #68: three outcomes, not two — see `classify_recv_error`. A
                // benign UDP reset used to land in the catch-all and cost a
                // 100ms stall per occurrence, during which the fleet's time
                // source answered nobody.
                Err(e) => {
                    let action = classify_recv_error(e.kind());
                    match action {
                        RecvErrorAction::Idle => {}
                        RecvErrorAction::Benign => debug!(
                            "[NTP-Server] Ignoring expected UDP condition: {} \
                             (a previous reply bounced; the loop is unaffected)",
                            e
                        ),
                        RecvErrorAction::Backoff => error!("[NTP-Server] Socket error: {}", e),
                    }
                    if let Some(delay) = action.backoff() {
                        std::thread::sleep(delay);
                    }
                }
            }
        }

        info!("[NTP-Server] Shutting down");
        Ok(())
    }

    /// Handle a single NTP request and send response.
    fn handle_request(&self, request: &[u8], src: SocketAddr) -> Result<()> {
        // Parse request header
        let li_vn_mode = request[0];
        let version = (li_vn_mode >> 3) & 0x07;
        let mode = li_vn_mode & 0x07;

        // Only respond to client requests (mode 3)
        if mode != MODE_CLIENT {
            debug!(
                "[NTP-Server] Ignoring non-client mode {} from {}",
                mode, src
            );
            return Ok(());
        }

        // Validate version (support v3 and v4)
        if !(3..=4).contains(&version) {
            debug!(
                "[NTP-Server] Ignoring unsupported version {} from {}",
                version, src
            );
            return Ok(());
        }

        // Get timestamps
        let receive_time = SystemTime::now();
        let (recv_secs, recv_frac) = system_time_to_ntp(receive_time);

        // Extract client's transmit timestamp (bytes 40-47 in request)
        // This becomes the originate timestamp in our response
        let originate_ts = &request[40..48];

        // Build response
        let response = self.build_response(version, originate_ts, recv_secs, recv_frac)?;

        // Send response
        self.socket.send_to(&response, src)?;
        debug!("[NTP-Server] Responded to {} (v{})", src, version);

        Ok(())
    }

    /// Build an NTP response packet.
    fn build_response(
        &self,
        version: u8,
        originate_ts: &[u8],
        recv_secs: u32,
        recv_frac: u32,
    ) -> Result<[u8; NTP_PACKET_SIZE]> {
        let mut response = [0u8; NTP_PACKET_SIZE];

        // Byte 0: LI (2 bits) | VN (3 bits) | Mode (3 bits)
        response[0] = (LI_NO_WARNING << 6) | (version << 3) | MODE_SERVER;

        // Byte 1: Stratum
        response[1] = self.stratum;

        // Byte 2: Poll interval (2^6 = 64 seconds typical)
        response[2] = 6;

        // Byte 3: Precision (2^-20 ≈ 1µs, typical for software clock)
        response[3] = 0xEC; // -20 as signed byte

        // Bytes 4-7: Root Delay (0 for local)
        // Already zero

        // Bytes 8-11: Root Dispersion (small value for local)
        response[8] = 0;
        response[9] = 0;
        response[10] = 0;
        response[11] = 16; // ~1ms dispersion

        // Bytes 12-15: Reference ID ("LOCL" for local clock)
        let ref_id = REF_ID_LOCL.to_be_bytes();
        response[12..16].copy_from_slice(&ref_id);

        // Bytes 16-23: Reference Timestamp — when we last actually re-read
        // upstream (#68), not when this process happened to start.
        let (ref_secs, ref_frac) = system_time_to_ntp(self.reference_timestamp());
        response[16..20].copy_from_slice(&ref_secs.to_be_bytes());
        response[20..24].copy_from_slice(&ref_frac.to_be_bytes());

        // Bytes 24-31: Originate Timestamp (copy client's transmit timestamp)
        response[24..32].copy_from_slice(originate_ts);

        // Bytes 32-39: Receive Timestamp (when we received the request)
        response[32..36].copy_from_slice(&recv_secs.to_be_bytes());
        response[36..40].copy_from_slice(&recv_frac.to_be_bytes());

        // Bytes 40-47: Transmit Timestamp (now)
        let transmit_time = SystemTime::now();
        let (tx_secs, tx_frac) = system_time_to_ntp(transmit_time);
        response[40..44].copy_from_slice(&tx_secs.to_be_bytes());
        response[44..48].copy_from_slice(&tx_frac.to_be_bytes());

        Ok(response)
    }
}

/// #68 — run the NTP server under supervision until `running` clears.
///
/// `NtpServer::run()` cannot exit early today (no `?`, no `break`, and every
/// receive error is classified rather than propagated), so in practice this
/// loops exactly once. The supervisor exists so the fleet's time source does not
/// DEPEND on that property surviving a future edit — and it therefore treats
/// **any** of the three ways the loop can stop while `running` is still set as
/// an unexpected exit worth restarting:
///
/// - `Err(..)` — a `?` added to the loop later;
/// - `Ok(())` while `running` is still set — a `break` added to the loop later.
///   This is the likelier regression, and reading it as "clean shutdown" would
///   have exited the supervisor silently, which is exactly the outcome it is
///   here to prevent;
/// - a **panic** — caught, so one bad packet cannot leave the daemon alive but
///   serving nothing.
///
/// The re-bound server inherits the original's status source, so a restart never
/// silently reverts to advertising its own restart time as the NTP reference
/// timestamp. A re-bind failure (port taken during the gap) is not fatal either:
/// it backs off and retries, so a transient collision heals itself.
pub fn run_supervised(server: NtpServer, running: Arc<AtomicBool>) {
    run_supervised_with(server, running, |srv, run_flag| srv.run(run_flag));
}

/// The supervision loop, with the "run it once" step injectable so a test can
/// reproduce an early exit that `run()` itself cannot currently produce.
fn run_supervised_with<F>(server: NtpServer, running: Arc<AtomicBool>, mut run_once: F)
where
    F: FnMut(&NtpServer, Arc<AtomicBool>) -> Result<()>,
{
    let port = server.port;
    let stratum = server.stratum;
    // Carried across restarts: `NtpServer::new()` would otherwise hand back a
    // server with `status: None`, silently reverting the reference-timestamp fix.
    let status_source = server.status.clone();
    // dantesync#52: carry the DSCP config so the rebind re-marks the fresh socket.
    let dscp = server.dscp.clone();
    let mut current = server;

    while running.load(Ordering::SeqCst) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_once(&current, running.clone())
        }));

        // A requested shutdown is the ONLY clean way out.
        if !running.load(Ordering::SeqCst) {
            break;
        }

        match outcome {
            Ok(Ok(())) => error!(
                "[NTP-Server] Loop returned while still running — restarting on port {}",
                port
            ),
            Ok(Err(e)) => error!(
                "[NTP-Server] Loop exited unexpectedly: {} — restarting on port {}",
                e, port
            ),
            Err(_) => error!(
                "[NTP-Server] Loop PANICKED — restarting on port {} (the daemon must \
                 never stay alive serving nothing)",
                port
            ),
        }

        std::thread::sleep(SUPERVISOR_RESTART_BACKOFF);

        loop {
            if !running.load(Ordering::SeqCst) {
                return;
            }
            match NtpServer::new(port, stratum, dscp.clone()) {
                Ok(mut fresh) => {
                    fresh.status = status_source.clone();
                    warn!("[NTP-Server] Restarted after an unexpected loop exit");
                    current = fresh;
                    break;
                }
                Err(e) => {
                    error!(
                        "[NTP-Server] Re-bind on port {} failed: {} — retrying",
                        port, e
                    );
                    std::thread::sleep(SUPERVISOR_RESTART_BACKOFF);
                }
            }
        }
    }

    info!("[NTP-Server] Supervisor stopped");
}

/// Backoff between an unexpected server-loop exit and its restart.
const SUPERVISOR_RESTART_BACKOFF: Duration = Duration::from_secs(1);

// ============================================================================
// NTP TIMESTAMP HELPERS
// ============================================================================

/// Convert SystemTime to NTP timestamp (seconds since 1900, fractional seconds).
fn system_time_to_ntp(time: SystemTime) -> (u32, u32) {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let unix_secs = duration.as_secs();
    let ntp_secs = unix_secs + NTP_EPOCH_OFFSET;

    // Fractional part: nanos -> 32-bit fraction
    // frac = nanos * 2^32 / 10^9
    let nanos = duration.subsec_nanos() as u64;
    let frac = ((nanos << 32) / 1_000_000_000) as u32;

    (ntp_secs as u32, frac)
}

/// Convert NTP timestamp to SystemTime.
#[allow(dead_code)]
fn ntp_to_system_time(secs: u32, frac: u32) -> SystemTime {
    let unix_secs = (secs as u64).saturating_sub(NTP_EPOCH_OFFSET);
    // frac * 10^9 / 2^32
    let nanos = ((frac as u64 * 1_000_000_000) >> 32) as u32;
    UNIX_EPOCH + Duration::new(unix_secs, nanos)
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idle_recv_blocks_for_read_timeout_not_busy_loop() {
        // #340: the NTP-server listen socket must be BLOCKING with a read timeout — NOT non-blocking.
        // set_nonblocking(true) and set_read_timeout(..) are mutually exclusive: non-blocking wins,
        // so recv_from returns WouldBlock INSTANTLY and the run() loop's WouldBlock arm `continue`s
        // with no sleep -> one thread busy-spins a full CPU core (observed on strih, the NTP master,
        // ~99% of one core continuously). With a blocking socket + 100ms read timeout, an idle
        // recv_from blocks in the kernel until the timeout, so the loop wakes ~10x/sec (graceful
        // shutdown still <=100ms) and consumes ~0% idle. This test pins that: an idle recv_from with
        // no inbound packet must take ~the read timeout, not return immediately.
        let server = NtpServer::new(0, 3, Default::default())
            .expect("bind ephemeral NTP server for the test");
        let mut buf = [0u8; NTP_PACKET_SIZE];
        let t0 = std::time::Instant::now();
        let _ = server.socket.recv_from(&mut buf); // nothing inbound -> blocks until the read timeout
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(90),
            "idle recv_from returned in {:?} (<90ms) — the listen socket is non-blocking, so the \
             NTP-server loop busy-spins a core (#340). It must be blocking with a ~100ms read timeout.",
            elapsed
        );
    }

    // ========================================================================
    // #68 — A BENIGN UDP RESET MUST NOT DEGRADE THE FLEET'S TIME SOURCE
    // ========================================================================
    // On Windows a UDP socket that has sent a datagram to a port with no
    // listener gets WSAECONNRESET (os error 10054) on its NEXT recvfrom — the
    // ICMP port-unreachable surfacing on a connectionless socket. It is normal
    // and expected for a server whose clients come and go. Today it lands in
    // run()'s catch-all arm: logged at error! severity and followed by a 100 ms
    // sleep, during which the master answers NOBODY. A burst of them degrades
    // the whole fleet's time source. (It does NOT kill the loop — verified
    // against the deployed v1.8.25 — so the fix is classification, not a
    // resurrection.)
    // ========================================================================

    #[test]
    fn classify_recv_error_treats_a_udp_reset_as_benign_68() {
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::ConnectionReset),
            RecvErrorAction::Benign,
            "WSAECONNRESET is an expected UDP condition, not a server fault"
        );
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::ConnectionRefused),
            RecvErrorAction::Benign,
            "the POSIX ICMP-unreachable equivalent is equally benign"
        );
    }

    #[test]
    fn classify_recv_error_keeps_idle_wakeups_free_68() {
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::WouldBlock),
            RecvErrorAction::Idle
        );
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::TimedOut),
            RecvErrorAction::Idle
        );
    }

    #[test]
    fn classify_recv_error_backs_off_only_on_a_genuinely_unknown_error_68() {
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::PermissionDenied),
            RecvErrorAction::Backoff
        );
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::AddrInUse),
            RecvErrorAction::Backoff
        );
    }

    #[test]
    fn a_benign_reset_never_stalls_the_server_68() {
        // The 100 ms stall is the actual harm: while sleeping, the fleet's time
        // source answers nobody.
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::ConnectionReset).backoff(),
            None
        );
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::TimedOut).backoff(),
            None
        );
        assert_eq!(
            classify_recv_error(std::io::ErrorKind::PermissionDenied).backoff(),
            Some(Duration::from_millis(100))
        );
    }

    /// The supervised server answers repeated client queries from one long-lived
    /// loop and shuts down promptly when the running flag clears — so a restart
    /// is never needed to get the NTP server back.
    #[test]
    fn run_supervised_serves_consecutive_requests_and_shuts_down_68() {
        use std::net::UdpSocket as StdUdpSocket;

        let server = NtpServer::new(0, 3, Default::default()).expect("bind ephemeral NTP server");
        let addr = server
            .local_addr()
            .expect("server must expose its bound addr");
        let port = addr.port();

        let running = Arc::new(AtomicBool::new(true));
        let server_running = running.clone();
        let handle = std::thread::spawn(move || run_supervised(server, server_running));

        let client = StdUdpSocket::bind("127.0.0.1:0").expect("bind client");
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("client read timeout");

        for attempt in 1..=2 {
            let mut request = [0u8; NTP_PACKET_SIZE];
            request[0] = (4 << 3) | MODE_CLIENT; // v4 client
            request[40 + attempt as usize] = 0xAB; // distinct originate stamp
            client
                .send_to(&request, ("127.0.0.1", port))
                .unwrap_or_else(|e| panic!("send #{} failed: {}", attempt, e));

            let mut response = [0u8; NTP_PACKET_SIZE];
            let (n, _) = client
                .recv_from(&mut response)
                .unwrap_or_else(|e| panic!("no response to request #{}: {}", attempt, e));
            assert_eq!(n, NTP_PACKET_SIZE, "short response to request #{}", attempt);
            assert_eq!(
                response[0] & 0x07,
                MODE_SERVER,
                "response #{} must be a server-mode packet",
                attempt
            );
            assert_eq!(
                &response[24..32],
                &request[40..48],
                "response #{} must echo this request's own originate timestamp",
                attempt
            );
        }

        running.store(false, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !handle.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            handle.is_finished(),
            "the supervised server must stop when the running flag clears"
        );
        handle.join().expect("supervisor thread panicked");
    }

    /// #68 review finding: the supervisor guarded the ONE exit that cannot
    /// happen and missed the two that can.
    ///
    /// `run()` has no `?` and no `break`, so its `Err` arm is unreachable — but
    /// the regression a future edit is most likely to introduce is exactly a
    /// `break`, which returns `Ok(())` and used to be read as "clean shutdown",
    /// exiting the supervisor silently. And a re-bound server was built with
    /// `NtpServer::new()`, which leaves `status: None` — so a restart silently
    /// reverted the reference-timestamp fix and served its own restart time
    /// forever after.
    #[test]
    fn an_early_return_while_still_running_restarts_with_the_status_source_68() {
        let mut server =
            NtpServer::new(0, 3, Default::default()).expect("bind ephemeral NTP server");
        server.set_status_source(Arc::new(std::sync::RwLock::new(
            crate::status::SyncStatus::default(),
        )));

        let running = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(std::sync::Mutex::new(0usize));

        let seen_status = Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
        let calls_inner = calls.clone();
        let seen_inner = seen_status.clone();
        let running_inner = running.clone();

        run_supervised_with(server, running.clone(), move |srv, _r| {
            let mut n = calls_inner.lock().expect("call counter");
            *n += 1;
            seen_inner
                .lock()
                .expect("status witness")
                .push(srv.status.is_some());
            if *n >= 2 {
                // Second pass: ask for a real shutdown so the test terminates.
                running_inner.store(false, Ordering::SeqCst);
            }
            // Return Ok WHILE `running` is still set on the first pass — the
            // shape a future `break` inside run()'s loop would produce.
            Ok(())
        });

        assert_eq!(
            *calls.lock().expect("call counter"),
            2,
            "an early Ok(()) while still running must restart the server, not exit"
        );
        let seen = seen_status.lock().expect("status witness");
        assert_eq!(
            seen.as_slice(),
            &[true, true],
            "the re-bound server must keep serving the real upstream reference \
             timestamp — a restart that drops the status source silently reverts it"
        );
    }

    /// #68: the reference timestamp is the "when did I last read my own
    /// reference" field of the NTP protocol. It was pinned to process start and
    /// never moved, which was honest only while the master genuinely never
    /// re-read upstream. Now that it does, the served value must follow the
    /// last real upstream sync.
    #[test]
    fn the_served_reference_timestamp_follows_the_last_real_upstream_sync_68() {
        let mut server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: UNIX_EPOCH + Duration::from_secs(1_000_000),
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };

        let status = Arc::new(std::sync::RwLock::new(crate::status::SyncStatus::default()));
        status.write().unwrap().ntp_updated_ts = 1_786_439_763;
        server.set_status_source(status);

        let response = server.build_response(4, &[0u8; 8], 100, 200).unwrap();
        let ref_secs = u32::from_be_bytes([response[16], response[17], response[18], response[19]]);
        assert_eq!(
            ref_secs as u64,
            1_786_439_763 + NTP_EPOCH_OFFSET,
            "served reference timestamp must be the last upstream sync, not process start"
        );
    }

    #[test]
    fn the_reference_timestamp_falls_back_when_upstream_was_never_read_68() {
        let created = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: created,
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };
        // Status present, but nothing measured yet (ntp_updated_ts == 0).
        server.set_status_source(Arc::new(std::sync::RwLock::new(
            crate::status::SyncStatus::default(),
        )));

        let response = server.build_response(4, &[0u8; 8], 100, 200).unwrap();
        let ref_secs = u32::from_be_bytes([response[16], response[17], response[18], response[19]]);
        assert_eq!(
            ref_secs as u64,
            1_000_000 + NTP_EPOCH_OFFSET,
            "never-measured must fall back to server start, never to the 1970 epoch"
        );
    }

    #[test]
    fn test_system_time_to_ntp_epoch() {
        // Unix epoch (1970-01-01 00:00:00) should be NTP epoch + 70 years
        let unix_epoch = UNIX_EPOCH;
        let (secs, _) = system_time_to_ntp(unix_epoch);
        assert_eq!(secs as u64, NTP_EPOCH_OFFSET);
    }

    #[test]
    fn test_system_time_to_ntp_roundtrip() {
        let original = SystemTime::now();
        let (secs, frac) = system_time_to_ntp(original);
        let recovered = ntp_to_system_time(secs, frac);

        // Should be within 1µs (due to fractional precision)
        let diff = original
            .duration_since(recovered)
            .or_else(|_| recovered.duration_since(original))
            .unwrap_or_default();
        assert!(diff.as_micros() < 10, "Roundtrip error: {:?}", diff);
    }

    #[test]
    fn test_ntp_fractional_conversion() {
        // Test 0.5 seconds
        let time = UNIX_EPOCH + Duration::new(0, 500_000_000);
        let (_, frac) = system_time_to_ntp(time);
        // 0.5 * 2^32 = 2147483648
        assert!(
            (frac as i64 - 2147483648).abs() < 1000,
            "Fractional 0.5s conversion: {}",
            frac
        );
    }

    #[test]
    fn test_ntp_fractional_quarter() {
        // Test 0.25 seconds
        let time = UNIX_EPOCH + Duration::new(0, 250_000_000);
        let (_, frac) = system_time_to_ntp(time);
        // 0.25 * 2^32 = 1073741824
        assert!(
            (frac as i64 - 1073741824).abs() < 1000,
            "Fractional 0.25s conversion: {}",
            frac
        );
    }

    #[test]
    fn test_ntp_packet_constants() {
        assert_eq!(NTP_PACKET_SIZE, 48);
        assert_eq!(NTP_EPOCH_OFFSET, 2_208_988_800);
        assert_eq!(LI_NO_WARNING, 0);
        assert_eq!(MODE_SERVER, 4);
        assert_eq!(MODE_CLIENT, 3);
    }

    #[test]
    fn test_ref_id_locl() {
        // "LOCL" in ASCII
        let bytes = REF_ID_LOCL.to_be_bytes();
        assert_eq!(bytes, [b'L', b'O', b'C', b'L']);
    }

    #[test]
    fn test_build_response_format() {
        // Create a mock server (won't actually bind in tests)
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: SystemTime::now(),
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };

        let originate_ts = [0u8; 8];
        let response = server.build_response(4, &originate_ts, 100, 200).unwrap();

        // Check header
        let li_vn_mode = response[0];
        let li = (li_vn_mode >> 6) & 0x03;
        let vn = (li_vn_mode >> 3) & 0x07;
        let mode = li_vn_mode & 0x07;

        assert_eq!(li, LI_NO_WARNING);
        assert_eq!(vn, 4); // Version 4
        assert_eq!(mode, MODE_SERVER);

        // Check stratum
        assert_eq!(response[1], 3);

        // Check reference ID
        assert_eq!(&response[12..16], b"LOCL");
    }

    #[test]
    fn test_response_copies_originate_timestamp() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: SystemTime::now(),
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };

        let originate_ts = [1, 2, 3, 4, 5, 6, 7, 8];
        let response = server.build_response(4, &originate_ts, 100, 200).unwrap();

        // Originate timestamp should be at bytes 24-31
        assert_eq!(&response[24..32], &originate_ts);
    }

    #[test]
    fn test_receive_timestamp_in_response() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: SystemTime::now(),
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };

        let recv_secs: u32 = 0x12345678;
        let recv_frac: u32 = 0xABCDEF00;
        let response = server
            .build_response(4, &[0; 8], recv_secs, recv_frac)
            .unwrap();

        // Receive timestamp at bytes 32-39
        let recv_secs_out =
            u32::from_be_bytes([response[32], response[33], response[34], response[35]]);
        let recv_frac_out =
            u32::from_be_bytes([response[36], response[37], response[38], response[39]]);

        assert_eq!(recv_secs_out, recv_secs);
        assert_eq!(recv_frac_out, recv_frac);
    }

    #[test]
    fn test_version_3_response() {
        let server = NtpServer {
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
            stratum: 3,
            port: 0,
            reference_time: SystemTime::now(),
            status: None,
            dscp: crate::dscp::DscpConfig::default(),
        };

        let response = server.build_response(3, &[0; 8], 100, 200).unwrap();

        let vn = (response[0] >> 3) & 0x07;
        assert_eq!(vn, 3, "Response should match client's version");
    }
}
