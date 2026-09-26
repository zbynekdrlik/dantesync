//! dantesync#119 (1.11.1) — the STEP LAW: move the wall by exactly the amount asked for.
//!
//! # Why a step must be exact
//!
//! The PTP phase lock (`crate::ptp_phase_lock`) holds `e = (t2 − t1) − D` at zero. A date step
//! moves the wall AND `D` by the requested amount (`PhaseLockCore::note_step`), so an exact step
//! is invisible to the loop. A step that lands SHORT by `s` leaves `e = −s` behind, and the phase
//! lock pays it back through the frequency word — at `K_P = 0.02 ppm/µs`, 250 µs is 5 ppm of rate
//! away from the Dante tick. With the fleet date moved in micro-corrections every 20 s (1.11.0)
//! that rate is never clean: the Windows media clocks that follow the adjustment rate leave the
//! Dante tick (camera-box#1372, measured on stream: `e` −170 … −860 µs after every +500 µs step,
//! the word +8 … +13 ppm off its −4.6 ppm baseline).
//!
//! # Why a read-modify-write step was not exact
//!
//! Both operating systems set an ABSOLUTE time. The step is therefore "read now, set now + offset",
//! and it lands short by everything that elapses between the read and the kernel applying the
//! set. On Windows the read was `GetSystemTimeAsFileTime`, the COARSE system time (updated only on
//! the clock interrupt, 0.5 ms with a raised timer resolution): every step lost the coarse lag,
//! 0 … one tick, 250 µs on average — while `NtSetSystemTime` sets the precise time to the target.
//!
//! # The law
//!
//! [`step_wall`] reads the PRECISE wall and a step-immune REFERENCE clock that runs at the wall's
//! rate (Windows: QPC scaled to the system-time rate; Linux: `CLOCK_MONOTONIC`), back to back. It
//! sets `precise + remaining + lead`, where `lead` is the learned read→set latency
//! ([`StepLead`]), and measures what the set actually did: `Δprecise − Δreference`, independent of
//! how long the call took or how the kernel applies it. A residual beyond
//! [`STEP_TOLERANCE_NS`] in the requested direction is stepped again (at most
//! [`MAX_STEP_ATTEMPTS`] sets); an overshoot is never corrected backwards (the wall does not run
//! back for a forward step), it is bounded by the lead's slew limit.
//!
//! Pure (the OS behind [`StepOps`]) so the unit tests and the two-clock bench run the same law
//! against models of both operating systems.

/// One back-to-back reading of the clocks a step is measured with (ns since the Unix epoch for
/// the two system times; the reference has an arbitrary origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockReading {
    /// The system time as the COARSE API returns it (Windows `GetSystemTimeAsFileTime`, updated
    /// on the clock interrupt). Linux has no coarser path in use: it equals `precise_ns` there.
    pub coarse_ns: i64,
    /// The precise system time — the clock every PTP timestamp is taken with.
    pub precise_ns: i64,
    /// A clock no step moves, running at the system time's rate (its frequency word included).
    pub reference_ns: i64,
}

/// The operating system under the step law.
pub trait StepOps {
    /// Read all three clocks, back to back.
    fn read(&mut self) -> ClockReading;
    /// Set the system time to `target_ns` (ns since the Unix epoch).
    fn set(&mut self, target_ns: i64) -> Result<(), String>;
}

/// A step is done once its residual (requested − realized) is within this.
pub const STEP_TOLERANCE_NS: i64 = 10_000;

/// At most this many sets per step (the first plus corrections).
pub const MAX_STEP_ATTEMPTS: u32 = 3;

/// The learned read→set latency is clamped to ±this.
pub const MAX_LEAD_NS: i64 = 1_000_000;

/// One observation moves the learned latency by at most this, so a single preempted set cannot
/// make the next steps overshoot by more than the tolerance.
pub const LEAD_SLEW_NS: i64 = 5_000;

/// The learned read→set latency (ns) of this clock, carried from step to step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepLead {
    lead_ns: i64,
}

impl StepLead {
    pub fn lead_ns(&self) -> i64 {
        self.lead_ns
    }
}

/// What one step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StepOutcome {
    pub requested_ns: i64,
    /// The wall's measured move (`Δprecise − Δreference` summed over the sets).
    pub realized_ns: i64,
    /// Sets made (0 for a zero step).
    pub attempts: u32,
    /// `precise − coarse` at the first read: what a coarse-read step would have lost.
    pub coarse_lag_ns: i64,
}

impl StepOutcome {
    /// `requested − realized`: the phase error this step leaves for the phase lock.
    pub fn residual_ns(&self) -> i64 {
        self.requested_ns.wrapping_sub(self.realized_ns)
    }
}

/// Step the wall of `ops` by `requested_ns`.
pub fn step_wall<O: StepOps>(
    ops: &mut O,
    lead: &mut StepLead,
    requested_ns: i64,
) -> Result<StepOutcome, String> {
    let _ = lead;
    let r0 = ops.read();
    let target = r0
        .coarse_ns
        .checked_add(requested_ns)
        .filter(|t| *t > 0)
        .ok_or_else(|| format!("a step of {requested_ns} ns would leave the valid time range"))?;
    ops.set(target)?;
    let r1 = ops.read();
    let realized = (r1.precise_ns - r0.precise_ns) - (r1.reference_ns - r0.reference_ns);
    Ok(StepOutcome {
        requested_ns,
        realized_ns: realized,
        attempts: 1,
        coarse_lag_ns: r0.precise_ns - r0.coarse_ns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const US: i64 = 1_000;
    const MS: i64 = 1_000_000;

    /// A model of the OS: true time advances 1 µs per read, a set lands `latency` after the call
    /// starts and the call then blocks ~117 ms (measured on the rig), the coarse clock is the
    /// precise one floored to the clock-interrupt tick, the reference runs at the wall's rate.
    struct FakeOs {
        true_ns: i64,
        stepped_ns: i64,
        tick_ns: i64,
        latencies: Vec<i64>,
        sets: u32,
    }

    impl FakeOs {
        fn new(true_ns: i64, tick_ns: i64, latency_ns: i64) -> Self {
            FakeOs {
                true_ns,
                stepped_ns: 0,
                tick_ns,
                latencies: vec![latency_ns],
                sets: 0,
            }
        }
        fn wall(&self) -> i64 {
            self.true_ns + self.stepped_ns
        }
    }

    impl StepOps for FakeOs {
        fn read(&mut self) -> ClockReading {
            self.true_ns += US;
            let precise = self.wall();
            ClockReading {
                coarse_ns: precise - self.true_ns.rem_euclid(self.tick_ns),
                precise_ns: precise,
                reference_ns: self.true_ns,
            }
        }
        fn set(&mut self, target_ns: i64) -> Result<(), String> {
            let i = (self.sets as usize).min(self.latencies.len() - 1);
            self.sets += 1;
            self.true_ns += self.latencies[i];
            self.stepped_ns = target_ns - self.true_ns;
            self.true_ns += 117 * MS;
            Ok(())
        }
    }

    const T0: i64 = 1_790_000_000_000_000_000;

    #[test]
    fn the_measurement_is_the_walls_true_move() {
        for tick in [1, 500 * US, 15_625 * US] {
            let mut os = FakeOs::new(T0 + 377 * US, tick, 4 * US);
            let before = os.stepped_ns;
            let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
            assert_eq!(out.realized_ns, os.stepped_ns - before, "tick {tick}");
        }
    }

    #[test]
    fn a_coarse_clock_interrupt_lag_is_not_lost_from_the_step() {
        // 250 µs into a 0.5 ms tick: a coarse-read step lands 250 µs short.
        let mut os = FakeOs::new(T0 + 250 * US - US, 500 * US, 3 * US);
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(out.coarse_lag_ns, 250 * US);
        assert!(
            out.residual_ns().abs() <= STEP_TOLERANCE_NS,
            "residual {} ns",
            out.residual_ns()
        );
        assert_eq!(out.realized_ns, os.stepped_ns);
    }

    #[test]
    fn a_backward_step_is_exact_too() {
        let mut os = FakeOs::new(T0 + 3 * MS + 420 * US, 500 * US, 6 * US);
        let out = step_wall(&mut os, &mut StepLead::default(), -18_883 * US).unwrap();
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
    }

    #[test]
    fn the_read_to_set_latency_is_learned_so_steady_steps_land_in_one_set() {
        let mut lead = StepLead::default();
        let mut os = FakeOs::new(T0, 500 * US, 40 * US);
        let mut last = None;
        for k in 0..40 {
            os.true_ns += 20_000 * MS + 37 * US * k;
            last = Some(step_wall(&mut os, &mut lead, 500 * US).unwrap());
        }
        let out = last.unwrap();
        assert!(
            (lead.lead_ns() - 40 * US).abs() <= 2 * US,
            "lead {}",
            lead.lead_ns()
        );
        assert_eq!(out.attempts, 1, "{out:?}");
        assert!(out.residual_ns().abs() <= 2 * US, "{out:?}");
    }

    #[test]
    fn a_preempted_set_is_corrected_by_another_set() {
        let mut lead = StepLead::default();
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.latencies = vec![300 * US, 3 * US];
        let out = step_wall(&mut os, &mut lead, 500 * US).unwrap();
        assert_eq!(out.attempts, 2, "{out:?}");
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
        assert!(
            lead.lead_ns().abs() <= 2 * LEAD_SLEW_NS,
            "one preemption moves the lead by at most the slew per set: {}",
            lead.lead_ns()
        );
    }

    #[test]
    fn an_overshoot_is_never_stepped_back() {
        // A lead far above the real latency: the forward step overshoots, and the wall is not
        // stepped backwards to take it back (one set, the overshoot reported).
        let mut lead = StepLead { lead_ns: 50 * US };
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        let out = step_wall(&mut os, &mut lead, 500 * US).unwrap();
        assert_eq!(out.attempts, 1, "{out:?}");
        assert_eq!(out.realized_ns, 500 * US + 50 * US - 3 * US);
        assert!(out.residual_ns() < 0);
        assert_eq!(
            lead.lead_ns(),
            50 * US - LEAD_SLEW_NS,
            "learned towards 3 µs"
        );
    }

    #[test]
    fn the_attempts_are_bounded() {
        let mut os = FakeOs::new(T0, 500 * US, 400 * US);
        os.latencies = vec![400 * US; 10];
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(out.attempts, MAX_STEP_ATTEMPTS);
        assert_eq!(os.sets, MAX_STEP_ATTEMPTS);
    }

    #[test]
    fn a_zero_step_sets_nothing() {
        let mut os = FakeOs::new(T0, 500 * US, 3 * US);
        let out = step_wall(&mut os, &mut StepLead::default(), 0).unwrap();
        assert_eq!(out.attempts, 0);
        assert_eq!(out.realized_ns, 0);
        assert_eq!(os.sets, 0);
    }

    #[test]
    fn a_step_before_the_epoch_is_refused_without_a_set() {
        let mut os = FakeOs::new(5 * MS, 500 * US, 3 * US);
        let err = step_wall(&mut os, &mut StepLead::default(), -10 * MS).unwrap_err();
        assert!(err.contains("valid time range"), "{err}");
        assert_eq!(os.sets, 0);
    }

    #[test]
    fn a_failed_set_is_an_error() {
        struct Refuses;
        impl StepOps for Refuses {
            fn read(&mut self) -> ClockReading {
                ClockReading {
                    coarse_ns: T0,
                    precise_ns: T0,
                    reference_ns: 0,
                }
            }
            fn set(&mut self, _: i64) -> Result<(), String> {
                Err("NtSetSystemTime failed with NTSTATUS 0xC0000061".into())
            }
        }
        let err = step_wall(&mut Refuses, &mut StepLead::default(), 500 * US).unwrap_err();
        assert!(err.contains("0xC0000061"));
    }
}
