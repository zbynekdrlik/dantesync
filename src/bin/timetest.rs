//! Test actual timestamp precision on Windows

#[cfg(windows)]
fn main() {
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    println!("=== Windows Timestamp Precision Test ===\n");

    // Test 1: SystemTime::now() resolution
    println!("Test 1: SystemTime::now() resolution");
    println!("Taking 100 consecutive samples...\n");

    let mut samples: Vec<u128> = Vec::with_capacity(100);
    for _ in 0..100 {
        let t = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        samples.push(t);
    }

    // Calculate deltas
    let mut deltas: Vec<u128> = Vec::new();
    let mut zero_deltas = 0;
    for i in 1..samples.len() {
        let delta = samples[i].saturating_sub(samples[i - 1]);
        if delta == 0 {
            zero_deltas += 1;
        } else {
            deltas.push(delta);
        }
    }

    if !deltas.is_empty() {
        deltas.sort();
        let min = deltas[0];
        let max = deltas[deltas.len() - 1];
        let median = deltas[deltas.len() / 2];

        println!("Results:");
        println!("  Zero deltas (same timestamp): {}", zero_deltas);
        println!("  Non-zero deltas: {}", deltas.len());
        println!("  Min delta: {} ns ({:.3} us)", min, min as f64 / 1000.0);
        println!("  Max delta: {} ns ({:.3} us)", max, max as f64 / 1000.0);
        println!(
            "  Median delta: {} ns ({:.3} us)",
            median,
            median as f64 / 1000.0
        );
    } else {
        println!("All samples had the same timestamp!");
    }

    // Test 2: Resolution with small sleep
    println!("\n\nTest 2: Minimum detectable time difference");
    let mut min_detectable = 0u128;
    for sleep_us in [1, 10, 50, 100, 500, 1000].iter() {
        let before = SystemTime::now();
        thread::sleep(Duration::from_micros(*sleep_us));
        let after = SystemTime::now();

        let diff = after.duration_since(before).unwrap().as_nanos();
        println!(
            "  Sleep {}us -> Measured {}ns ({:.1}us)",
            sleep_us,
            diff,
            diff as f64 / 1000.0
        );

        if min_detectable == 0 && diff > 0 {
            min_detectable = diff;
        }
    }

    // Test 3: Instant precision (for comparison)
    println!("\n\nTest 3: Instant (QueryPerformanceCounter) resolution");
    let mut instant_samples: Vec<u128> = Vec::with_capacity(100);
    let base = Instant::now();
    for _ in 0..100 {
        instant_samples.push(base.elapsed().as_nanos());
    }

    let mut instant_deltas: Vec<u128> = Vec::new();
    let mut instant_zeros = 0;
    for i in 1..instant_samples.len() {
        let delta = instant_samples[i].saturating_sub(instant_samples[i - 1]);
        if delta == 0 {
            instant_zeros += 1;
        } else {
            instant_deltas.push(delta);
        }
    }

    if !instant_deltas.is_empty() {
        instant_deltas.sort();
        println!("  Zero deltas: {}", instant_zeros);
        println!("  Min delta: {} ns", instant_deltas[0]);
        println!(
            "  Max delta: {} ns",
            instant_deltas[instant_deltas.len() - 1]
        );
        println!(
            "  Median delta: {} ns",
            instant_deltas[instant_deltas.len() / 2]
        );
    }

    // Test 4: GetSystemTimePreciseAsFileTime precision
    println!("\n\nTest 4: GetSystemTimePreciseAsFileTime resolution");
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;

    let mut ft_samples: Vec<u64> = Vec::with_capacity(100);
    for _ in 0..100 {
        let ft = unsafe { GetSystemTimePreciseAsFileTime() };
        let val = (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
        ft_samples.push(val);
    }

    let mut ft_deltas: Vec<u64> = Vec::new();
    let mut ft_zeros = 0;
    for i in 1..ft_samples.len() {
        let delta = ft_samples[i].saturating_sub(ft_samples[i - 1]);
        if delta == 0 {
            ft_zeros += 1;
        } else {
            ft_deltas.push(delta);
        }
    }

    if !ft_deltas.is_empty() {
        ft_deltas.sort();
        // FILETIME is in 100ns units
        println!("  Zero deltas: {}", ft_zeros);
        println!(
            "  Min delta: {} (100ns units) = {} ns",
            ft_deltas[0],
            ft_deltas[0] * 100
        );
        println!(
            "  Max delta: {} (100ns units) = {} ns",
            ft_deltas[ft_deltas.len() - 1],
            ft_deltas[ft_deltas.len() - 1] * 100
        );
        println!(
            "  Median delta: {} (100ns units) = {} ns",
            ft_deltas[ft_deltas.len() / 2],
            ft_deltas[ft_deltas.len() / 2] * 100
        );
    }

    println!("\n=== Test Complete ===");
}

#[cfg(not(windows))]
fn main() {
    println!("This test only runs on Windows");
}
