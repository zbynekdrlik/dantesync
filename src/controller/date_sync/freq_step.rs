//! camera-box issue 1372 — the controller side of the phase lock's grandmaster FREQUENCY-STEP
//! follow: one log line per followed step and its wall time for `/status`. The detection and the
//! re-seed live in the pure core (`crate::ptp_phase_lock::freq_step`); nothing here touches the
//! clock, `D` or the date layer.

use super::*;
use crate::ptp_phase_lock::{FreqStep, FSTEP_PULL_TAU_S};

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// The phase lock followed a grandmaster frequency step in this window: log it once and keep
    /// its time (the status write that ends the window publishes it).
    pub(super) fn note_freq_step(&mut self, step: Option<FreqStep>) {
        let Some(s) = step else {
            return;
        };
        self.date_sync.last_freq_step_ts = Some((wall_now_ns() / 1_000_000_000).max(0) as u64);
        let bounded = if (s.step_ppm + s.error_ppm).abs() > 1e-9 {
            format!(" (measured {:+.1}ppm, bounded per event)", -s.error_ppm)
        } else {
            String::new()
        };
        info!(
            "[PHASE-LOCK] frequency step: {:+.1}ppm over {:.0}s (fit σ {:.2}ppm, linearity F {:.1}){} \
             -- integrator re-seeded I {:+.3} -> {:+.3}ppm; the {:+.0}us it left is pulled back \
             over ~{:.0}s, no wall step, D unchanged",
            s.step_ppm,
            s.span_s,
            s.sigma_ppm,
            s.linearity_f,
            bounded,
            s.integrator_before_ppm,
            s.integrator_after_ppm,
            s.pull_us,
            FSTEP_PULL_TAU_S * 3.0
        );
    }
}
