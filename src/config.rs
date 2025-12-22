use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub servo: ServoConfig,
    pub filters: FilterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServoConfig {
    pub kp: f64,
    pub ki: f64,
    pub max_freq_adj_ppm: f64,
    pub max_integral_ppm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub step_threshold_ns: i64,      // "MASSIVE_DRIFT_THRESHOLD_NS"
    pub panic_threshold_ns: i64,     // "MAX_PHASE_OFFSET_FOR_STEP_NS"
    pub sample_window_size: usize,
    pub min_delta_ns: i64,
    pub calibration_samples: usize,  // Number of samples for timestamp calibration (0 = disabled)
    pub ptp_stepping_enabled: bool,  // Enable stepping based on PTP offset (disable for frequency-only sync)
}

impl Default for SystemConfig {
    fn default() -> Self {
        #[cfg(windows)]
        {
            // WINDOWS CONFIGURATION
            // Uses SetSystemTimeAdjustmentPrecise for frequency adjustment (with inverted sign).
            // Frequency adjustment IS working - use stepping only for initial alignment (>50ms offset)
            // and as fallback if offset exceeds 5ms during operation.
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.0005,   // Same as Linux - P gain for offset correction
                    ki: 0.00005,  // Same as Linux - I gain for drift correction
                    max_freq_adj_ppm: 500.0,    // Same as Linux
                    max_integral_ppm: 100.0,    // Same as Linux
                },
                filters: FilterConfig {
                    // Windows needs much larger sample window due to high timestamp jitter
                    step_threshold_ns: 5_000_000,  // 5ms - only step if freq adjustment can't keep up
                    panic_threshold_ns: 50_000_000, // 50ms - initial coarse step threshold
                    sample_window_size: 16, // 16 samples with median selection for jitter rejection
                    min_delta_ns: 0, // No minimum (Windows timestamps are less precise)
                    calibration_samples: 0,
                    ptp_stepping_enabled: true, // Enable stepping for initial alignment and large drifts
                },
            }
        }

        #[cfg(not(windows))]
        {
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.0005,
                    ki: 0.00005,
                    max_freq_adj_ppm: 500.0,    
                    max_integral_ppm: 100.0,
                },
                filters: FilterConfig {
                    step_threshold_ns: 5_000_000,  // 5ms
                    panic_threshold_ns: 10_000_000, // 10ms
                    sample_window_size: 4,
                    min_delta_ns: 1_000_000, // 1ms
                    calibration_samples: 0, // Linux uses kernel timestamping, no calibration needed
                    ptp_stepping_enabled: true, // Linux kernel timestamps are accurate, stepping works
                },
            }
        }
    }
}
