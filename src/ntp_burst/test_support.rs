//! dantesync#136 — a slow NTP upstream on loopback, for the tests of the burst off the loop.

use crate::ntp_server::NtpServer;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A real [`NtpServer`] behind a relay that holds every query `round_trip` (half on the way in,
/// half on the way out — a loaded NIC or a slow master): a burst of `NTP_BURST_SIZE` queries takes
/// about `NTP_BURST_SIZE × round_trip`, and the measured offset stays near zero. Stops when
/// dropped.
pub(crate) struct SlowNtpUpstream {
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl SlowNtpUpstream {
    pub(crate) fn start(round_trip: Duration) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let server = NtpServer::new(0, 3, crate::dscp::DscpConfig::default())
            .expect("an NTP server on an ephemeral port");
        let server_port = server.local_addr().expect("its address").port();
        let server_running = running.clone();
        let server_thread = thread::spawn(move || {
            let _ = server.run(server_running);
        });

        let relay = UdpSocket::bind("127.0.0.1:0").expect("the relay's socket");
        relay
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let addr = relay.local_addr().unwrap();
        let upstream = UdpSocket::bind("127.0.0.1:0").expect("the relay's upstream socket");
        upstream
            .connect(("127.0.0.1", server_port))
            .expect("connect to the NTP server");
        upstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let half = round_trip / 2;
        let relay_running = running.clone();
        let relay_thread = thread::spawn(move || {
            let mut buf = [0u8; 128];
            let mut reply = [0u8; 128];
            while relay_running.load(Ordering::SeqCst) {
                let Ok((n, client)) = relay.recv_from(&mut buf) else {
                    continue;
                };
                thread::sleep(half);
                if upstream.send(&buf[..n]).is_err() {
                    continue;
                }
                if let Ok(m) = upstream.recv(&mut reply) {
                    thread::sleep(half);
                    let _ = relay.send_to(&reply[..m], client);
                }
            }
        });
        SlowNtpUpstream {
            addr,
            running,
            threads: vec![server_thread, relay_thread],
        }
    }

    /// The upstream as an `NtpClient` server name (`127.0.0.1:<port>`).
    pub(crate) fn server_name(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for SlowNtpUpstream {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}
