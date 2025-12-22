//! Simple PTP packet logger - shows raw T1, T2, and offset without filtering

use std::collections::HashMap;
use std::net::UdpSocket;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    println!("=== PTP Raw Offset Logger ===\n");

    // Create sockets for PTP ports
    let sock_event = UdpSocket::bind("0.0.0.0:319").expect("Failed to bind port 319");
    let sock_general = UdpSocket::bind("0.0.0.0:320").expect("Failed to bind port 320");

    sock_event.set_nonblocking(true).unwrap();
    sock_general.set_nonblocking(true).unwrap();

    // Join multicast
    let mcast = "224.0.1.129".parse().unwrap();
    let any = "0.0.0.0".parse().unwrap();
    let _ = sock_event.join_multicast_v4(&mcast, &any);
    let _ = sock_general.join_multicast_v4(&mcast, &any);

    println!("Listening on ports 319/320...\n");
    println!("{:>6} {:>20} {:>20} {:>12} {:>12}", "Seq", "T1 (ns mod 1s)", "T2 (ns mod 1s)", "Offset (us)", "Raw (us)");
    println!("{}", "-".repeat(80));

    // Store pending syncs: seq_id -> (T2_ns, raw_T2)
    let mut pending: HashMap<u16, (i64, i64)> = HashMap::new();
    let mut buf = [0u8; 2048];
    let mut count = 0;
    let mut offsets: Vec<f64> = Vec::new();

    while count < 50 {
        // Check event socket (Sync messages)
        if let Ok((size, _)) = sock_event.recv_from(&mut buf) {
            let t2 = SystemTime::now();
            let t2_ns = t2.duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64;

            if size >= 36 && (buf[0] & 0x0F) == 1 {  // PTPv1
                let msg_type = buf[32] & 0x0F;
                if msg_type == 0 {  // Sync
                    let seq_id = u16::from_be_bytes([buf[30], buf[31]]);
                    pending.insert(seq_id, (t2_ns, t2_ns));
                }
            }
        }

        // Check general socket (FollowUp messages)
        if let Ok((size, _)) = sock_general.recv_from(&mut buf) {
            if size >= 52 && (buf[0] & 0x0F) == 1 {  // PTPv1
                let msg_type = buf[32] & 0x0F;
                if msg_type == 2 {  // FollowUp
                    let assoc_seq = u16::from_be_bytes([buf[42], buf[43]]);

                    if let Some((t2_ns, _)) = pending.remove(&assoc_seq) {
                        // Parse T1 from FollowUp (offset 44-51 in body, which starts at 36)
                        let t1_secs = u32::from_be_bytes([buf[44], buf[45], buf[46], buf[47]]) as i64;
                        let t1_nanos = u32::from_be_bytes([buf[48], buf[49], buf[50], buf[51]]) as i64;
                        let t1_ns = t1_secs * 1_000_000_000 + t1_nanos;

                        // Calculate phase offset (within 1 second)
                        let t1_mod = t1_ns % 1_000_000_000;
                        let t2_mod = t2_ns % 1_000_000_000;

                        let mut raw_offset = t2_mod - t1_mod;
                        // Normalize to ±0.5s
                        if raw_offset > 500_000_000 { raw_offset -= 1_000_000_000; }
                        if raw_offset < -500_000_000 { raw_offset += 1_000_000_000; }

                        let offset_us = raw_offset as f64 / 1000.0;
                        offsets.push(offset_us);

                        println!("{:>6} {:>20} {:>20} {:>+12.1} {:>+12.1}",
                                 assoc_seq, t1_mod, t2_mod, offset_us, offset_us);

                        count += 1;
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(1));
    }

    // Statistics
    if offsets.len() > 5 {
        offsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let min = offsets[0];
        let max = offsets[offsets.len()-1];
        let median = offsets[offsets.len()/2];
        let mean: f64 = offsets.iter().sum::<f64>() / offsets.len() as f64;

        let variance: f64 = offsets.iter()
            .map(|x| (x - mean).powi(2))
            .sum::<f64>() / offsets.len() as f64;
        let std_dev = variance.sqrt();

        println!("\n{}", "=".repeat(80));
        println!("Statistics ({} samples):", offsets.len());
        println!("  Min offset:    {:+.1} us", min);
        println!("  Max offset:    {:+.1} us", max);
        println!("  Median offset: {:+.1} us", median);
        println!("  Mean offset:   {:+.1} us", mean);
        println!("  Std deviation: {:.1} us", std_dev);
        println!("  Range:         {:.1} us", max - min);
    }

    println!("\n=== Done ===");
}
