//! Test UDP receive latency and jitter on Windows

use std::net::UdpSocket;
use std::time::{Duration, Instant};

fn main() {
    println!("=== UDP Receive Latency Test ===\n");

    // Bind to PTP event port
    let sock = match UdpSocket::bind("0.0.0.0:319") {
        Ok(s) => s,
        Err(e) => {
            println!("Failed to bind to port 319: {}", e);
            println!("Trying port 31900 instead...");
            UdpSocket::bind("0.0.0.0:31900").expect("Failed to bind")
        }
    };

    sock.set_nonblocking(false).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();

    // Join multicast if possible
    let _ = sock.join_multicast_v4(&"224.0.1.129".parse().unwrap(), &"0.0.0.0".parse().unwrap());

    println!("Listening for PTP packets...");
    println!("Measuring time between consecutive packet receives.\n");

    let mut buf = [0u8; 2048];
    let mut last_recv = Instant::now();
    let mut deltas: Vec<u128> = Vec::new();
    let mut count = 0;

    // Measure inter-packet timing for 100 packets
    while count < 100 {
        match sock.recv_from(&mut buf) {
            Ok((size, _)) => {
                let now = Instant::now();
                if count > 0 {
                    let delta = now.duration_since(last_recv).as_nanos();
                    deltas.push(delta);

                    if count <= 10 || count % 20 == 0 {
                        println!(
                            "Packet {}: size={}, delta={:.3}ms",
                            count,
                            size,
                            delta as f64 / 1_000_000.0
                        );
                    }
                }
                last_recv = now;
                count += 1;
            }
            Err(e) => {
                println!("Recv error: {}", e);
                break;
            }
        }
    }

    if deltas.len() > 10 {
        deltas.sort();
        let min = deltas[0];
        let max = deltas[deltas.len() - 1];
        let median = deltas[deltas.len() / 2];
        let mean: u128 = deltas.iter().sum::<u128>() / deltas.len() as u128;

        // Calculate standard deviation
        let variance: f64 = deltas
            .iter()
            .map(|&x| {
                let diff = x as f64 - mean as f64;
                diff * diff
            })
            .sum::<f64>()
            / deltas.len() as f64;
        let std_dev = variance.sqrt();

        println!("\n=== Results ({} samples) ===", deltas.len());
        println!(
            "Min inter-packet time:    {:.3} ms",
            min as f64 / 1_000_000.0
        );
        println!(
            "Max inter-packet time:    {:.3} ms",
            max as f64 / 1_000_000.0
        );
        println!(
            "Median inter-packet time: {:.3} ms",
            median as f64 / 1_000_000.0
        );
        println!(
            "Mean inter-packet time:   {:.3} ms",
            mean as f64 / 1_000_000.0
        );
        println!(
            "Std deviation:            {:.3} ms ({:.1} us)",
            std_dev / 1_000_000.0,
            std_dev / 1000.0
        );

        // This std dev represents the jitter in packet arrival timing
        println!(
            "\n>>> Packet arrival jitter (std dev): {:.1} us",
            std_dev / 1000.0
        );
    }

    println!("\n=== Test Complete ===");
}
