//! Standalone Windows clock adjustment test
//! Tests if SetSystemTimeAdjustmentPrecise actually works

#[cfg(windows)]
fn main() {
    use std::time::Duration;
    use std::thread;
    use windows::Win32::System::SystemInformation::{
        GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise, GetSystemTimeAsFileTime,
    };
    use windows::Win32::System::Performance::{QueryPerformanceFrequency, QueryPerformanceCounter};
    use windows::Win32::Foundation::BOOL;

    println!("=== Windows Clock Adjustment Test ===\n");

    // Get performance counter frequency
    let mut perf_freq: i64 = 0;
    unsafe {
        QueryPerformanceFrequency(&mut perf_freq).expect("Failed to get perf freq");
    }
    println!("Performance Counter Frequency: {} Hz", perf_freq);

    // Get initial adjustment values
    let mut adj = 0u64;
    let mut inc = 0u64;
    let mut disabled = BOOL(0);
    unsafe {
        GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled).expect("Failed to get adjustment");
    }
    println!("Nominal Increment: {}", inc);
    println!("Current Adjustment: {}", adj);
    println!("Adjustment Disabled: {}", disabled.as_bool());
    println!();

    // Test function
    let test_ppm = |ppm: f64, duration_secs: u64| {
        println!("--- Testing {:+.0} PPM for {} seconds ---", ppm, duration_secs);

        // Calculate new adjustment
        let delta = (ppm * perf_freq as f64 / 1_000_000.0).round() as i64;
        let new_adj = (inc as i64 + delta) as u64;
        println!("Setting adjustment to {} (delta {:+})", new_adj, delta);

        // Get baseline
        let (start_pc, start_ft) = unsafe {
            let mut pc: i64 = 0;
            QueryPerformanceCounter(&mut pc).expect("QPC failed");
            let ft = GetSystemTimeAsFileTime();
            let ft_u64 = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);
            (pc, ft_u64)
        };

        // Apply adjustment
        unsafe {
            SetSystemTimeAdjustmentPrecise(new_adj, false).expect("Failed to set adjustment");
        }

        // Verify it was set
        unsafe {
            let mut verify = 0u64;
            let mut vi = 0u64;
            let mut vd = BOOL(0);
            GetSystemTimeAdjustmentPrecise(&mut verify, &mut vi, &mut vd).ok();
            println!("Verified adjustment: {} (expected {})", verify, new_adj);
        }

        // Wait
        thread::sleep(Duration::from_secs(duration_secs));

        // Get end measurements
        let (end_pc, end_ft) = unsafe {
            let mut pc: i64 = 0;
            QueryPerformanceCounter(&mut pc).expect("QPC failed");
            let ft = GetSystemTimeAsFileTime();
            let ft_u64 = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);
            (pc, ft_u64)
        };

        // Calculate
        let pc_elapsed = end_pc - start_pc;
        let wall_ns = (pc_elapsed as f64 / perf_freq as f64) * 1_000_000_000.0;

        let ft_elapsed = end_ft - start_ft;
        let system_ns = ft_elapsed as f64 * 100.0; // FILETIME is 100ns units

        let diff_ns = system_ns - wall_ns;
        let observed_ppm = (diff_ns / wall_ns) * 1_000_000.0;

        println!("Wall time (QPC):    {:.6} seconds", wall_ns / 1_000_000_000.0);
        println!("System time (FT):   {:.6} seconds", system_ns / 1_000_000_000.0);
        println!("Difference:         {:.3} ms", diff_ns / 1_000_000.0);
        println!("Requested PPM:      {:+.1}", ppm);
        println!("Observed PPM:       {:+.1}", observed_ppm);

        let effectiveness = if ppm.abs() > 0.1 { observed_ppm / ppm } else { 0.0 };
        println!("Effectiveness:      {:.1}%", effectiveness * 100.0);
        println!();

        observed_ppm
    };

    // Test 1: Baseline (nominal)
    println!("\n========== TEST 1: BASELINE (0 PPM) ==========");
    let baseline = test_ppm(0.0, 5);
    println!(">>> Baseline drift: {:+.1} PPM", baseline);

    // Reset to nominal
    unsafe { SetSystemTimeAdjustmentPrecise(inc, false).ok(); }
    thread::sleep(Duration::from_millis(500));

    // Test 2: Positive PPM
    println!("\n========== TEST 2: +500 PPM ==========");
    let pos = test_ppm(500.0, 5);
    let pos_corrected = pos - baseline;
    println!(">>> Observed change from baseline: {:+.1} PPM (expected +500)", pos_corrected);

    // Reset to nominal
    unsafe { SetSystemTimeAdjustmentPrecise(inc, false).ok(); }
    thread::sleep(Duration::from_millis(500));

    // Test 3: Negative PPM
    println!("\n========== TEST 3: -500 PPM ==========");
    let neg = test_ppm(-500.0, 5);
    let neg_corrected = neg - baseline;
    println!(">>> Observed change from baseline: {:+.1} PPM (expected -500)", neg_corrected);

    // Reset to nominal
    unsafe { SetSystemTimeAdjustmentPrecise(inc, false).ok(); }

    // Summary
    println!("\n========== SUMMARY ==========");
    println!("Baseline drift:         {:+.1} PPM", baseline);
    println!("+500 PPM test:          {:+.1} PPM observed ({:+.1} from baseline)", pos, pos_corrected);
    println!("-500 PPM test:          {:+.1} PPM observed ({:+.1} from baseline)", neg, neg_corrected);

    let avg_effectiveness = ((pos_corrected / 500.0).abs() + (neg_corrected / -500.0).abs()) / 2.0;
    println!("\nAverage effectiveness:  {:.1}%", avg_effectiveness * 100.0);

    if avg_effectiveness > 0.8 {
        println!("\n✓ FREQUENCY ADJUSTMENT IS WORKING!");
    } else if avg_effectiveness > 0.3 {
        println!("\n⚠ FREQUENCY ADJUSTMENT PARTIALLY WORKING");
    } else {
        println!("\n✗ FREQUENCY ADJUSTMENT NOT WORKING");
    }

    println!("\nClock reset to nominal.");
}

#[cfg(not(windows))]
fn main() {
    println!("This test only runs on Windows");
}
