use anyhow::Result;
use clap::Parser;
use log::{info, warn, error};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

mod ptp;
mod net;
mod clock;

use ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody};
use clock::SystemClock;
use std::io::ErrorKind;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Interface name to bind to (optional, auto-detected if not provided)
    #[arg(short, long)]
    interface: Option<String>,
}

struct PendingSync {
    // rx_time: Instant, // Unused
    rx_time_sys: std::time::SystemTime,
    source_uuid: [u8; 6],
}

fn main() -> Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    let _args = Args::parse(); // interface currently auto-detected, args unused for now

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        info!("Ctrl+C received. Shutting down...");
        r.store(false, Ordering::SeqCst);
    })?;

    // 1. Initialize Clock
    let mut sys_clock: Box<dyn SystemClock> = match clock::PlatformClock::new() {
        Ok(c) => Box::new(c),
        Err(e) => {
            error!("Failed to initialize system clock adjustment: {}", e);
            return Err(e);
        }
    };
    info!("System clock control initialized.");

    // 2. Network Interface
    let (iface, iface_ip) = net::get_default_interface()?;
    info!("Selected Interface: {} ({})", iface.name, iface_ip);
    
    // 3. Sockets
    let sock_event = net::create_multicast_socket(ptp::PTP_EVENT_PORT, iface_ip)?;
    let sock_general = net::create_multicast_socket(ptp::PTP_GENERAL_PORT, iface_ip)?;
    
    info!("Listening on 224.0.1.129 ports 319 (Event) and 320 (General)");

    // 4. Main Loop
    let mut buf = [0u8; 2048];
    let mut pending_syncs: HashMap<u16, PendingSync> = HashMap::new();
    
    // State for filtering
    let mut prev_t1_ns: i64 = 0;
    let mut prev_t2_ns: i64 = 0;
    let mut smoothed_factor = 1.0;
    let alpha = 0.005;
    let mut valid_count = 0;
    let settling_threshold = 10;
    let mut clock_settled = false;

    let mut last_log = Instant::now();
    
    while running.load(Ordering::SeqCst) {
        // Logging
        if last_log.elapsed() >= Duration::from_secs(10) {
            let adj_ppm = (smoothed_factor - 1.0) * 1_000_000.0;
            if !clock_settled {
                info!("[Status] Settling... ({}/{})", valid_count, settling_threshold);
            } else {
                info!("[Status] Factor: {:.9} ({:+.3} ppm). Adjustment active.", smoothed_factor, adj_ppm);
            }
            last_log = Instant::now();
        }

        // Poll sockets
        let sockets = [&sock_event, &sock_general];
        let mut did_work = false;

        for sock in sockets {
            match sock.recv_from(&mut buf) {
                Ok((size, _src)) => {
                    did_work = true;
                    // Capture T2 immediately
                    let t2 = std::time::SystemTime::now();

                    if size < ptp::PtpV1Header::SIZE {
                        continue;
                    }
                    
                    let header = match PtpV1Header::parse(&buf[..size]) {
                        Ok(h) => h,
                        Err(_) => continue,
                    };

                    match header.message_type {
                        PtpV1Control::Sync => {
                            // Store Sync T2
                            pending_syncs.insert(header.sequence_id, PendingSync {
                                rx_time_sys: t2,
                                source_uuid: header.source_uuid,
                            });
                        }
                        PtpV1Control::FollowUp => {
                            // Parse body
                            if let Ok(body) = PtpV1FollowUpBody::parse(&buf[ptp::PtpV1Header::SIZE..size]) {
                                if let Some(sync_info) = pending_syncs.remove(&body.associated_sequence_id) {
                                    if sync_info.source_uuid == header.source_uuid {
                                        // Process Valid Pair
                                        let t1_ns = body.precise_origin_timestamp.to_nanos();
                                        
                                        let t2_ns = sync_info.rx_time_sys.duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_nanos() as i64;

                                        valid_count += 1;
                                        if valid_count > settling_threshold {
                                            clock_settled = true;
                                        }

                                        if prev_t1_ns > 0 && prev_t2_ns > 0 {
                                            // Ensure monotonicity
                                            if t1_ns > prev_t1_ns && t2_ns > prev_t2_ns {
                                                let delta_master = t1_ns - prev_t1_ns;
                                                let delta_slave = t2_ns - prev_t2_ns;

                                                if delta_master > 0 && delta_slave > 0 {
                                                    let current_factor = delta_master as f64 / delta_slave as f64;
                                                    
                                                    // Sanity check (max 10% drift)
                                                    if (current_factor - 1.0).abs() < 0.1 {
                                                        smoothed_factor = alpha * current_factor + (1.0 - alpha) * smoothed_factor;
                                                        
                                                        if clock_settled {
                                                            if let Err(e) = sys_clock.adjust_frequency(smoothed_factor) {
                                                                warn!("Clock adjustment failed: {}", e);
                                                            }
                                                        }
                                                    } else {
                                                        warn!("Excessive drift detected (factor {}). Resetting filter.", current_factor);
                                                        smoothed_factor = 1.0;
                                                        valid_count = 0;
                                                        clock_settled = false;
                                                    }
                                                }
                                            } else {
                                                warn!("Time went backwards or duplicate packet. Resetting.");
                                                valid_count = 0;
                                                clock_settled = false;
                                                smoothed_factor = 1.0;
                                            }
                                        }
                                        
                                        prev_t1_ns = t1_ns;
                                        prev_t2_ns = t2_ns;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    // Nothing
                }
                Err(e) => {
                    warn!("Socket recv error: {}", e);
                }
            }
        }

        if !did_work {
            thread::sleep(Duration::from_millis(1));
        }
        
        // Cleanup old pending syncs
        if pending_syncs.len() > 100 {
            let now_sys = std::time::SystemTime::now();
            pending_syncs.retain(|_, v| now_sys.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5));
        }
    }

    info!("Exiting.");
    Ok(())
}
