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
//! the clock interrupt): every step lost the coarse lag, 0 … one tick (the tick on stream is not
//! measured; the always-negative error after each step, paid back by the phase lock and so
//! accumulating to the −170 … −860 µs seen, fits a 0.5 ms tick) — while `NtSetSystemTime` sets the
//! precise time to the target.
//!
//! # The law
//!
//! [`step_wall`] reads the PRECISE wall and a step-immune REFERENCE clock that runs at the wall's
//! rate (Windows: QPC scaled to the system-time rate; Linux: `CLOCK_MONOTONIC`) — the reference
//! on both sides of the wall, a reading preempted in between read again ([`read_tight`]). It sets
//! `precise + remaining + lead`, where `lead` is the learned read→set latency ([`StepLead`]), and
//! measures what the set actually did: `Δprecise − Δreference`, independent of how long the call
//! took or how the kernel applies it. A residual beyond [`STEP_TOLERANCE_NS`] in the requested
//! direction is stepped again (at most [`MAX_STEP_ATTEMPTS`] sets); an overshoot is never corrected
//! backwards (the wall does not run back for a forward step), and a residual beyond
//! [`MAX_CORRECTION_NS`] is never chased (the measurement is then wrong). Once a set has landed the
//! step is reported as made, with why it stopped short if it did ([`StepOutcome::stopped`]), so the
//! caller moves `D` with the wall.
//!
//! The on-rig check that the kernel applies a set as modelled: every `[StepClock]` line shows
//! `1 set(s)` and a residual within the tolerance, and `/status.date_step_phase_jump_us` reads a
//! few µs.
//!
//! Pure (the OS behind [`StepOps`]) so the unit tests and the two-clock bench run the same law
//! against models of both operating systems.

/// One reading of the clocks a step is measured with (ns since the Unix epoch for the two system
/// times; the reference has an arbitrary origin). The reference is read on BOTH sides of the
/// system times: `reference_ns` is the midpoint, `window_ns` how far apart the two reads were —
/// a read the scheduler preempted in the middle has a wide window and cannot be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockReading {
    /// The system time as the COARSE API returns it (Windows `GetSystemTimeAsFileTime`, updated
    /// on the clock interrupt). Linux has no coarser path in use: it equals `precise_ns` there.
    pub coarse_ns: i64,
    /// The precise system time — the clock every PTP timestamp is taken with.
    pub precise_ns: i64,
    /// A clock no step moves, running at the system time's rate (its frequency word included):
    /// the midpoint of its reads before and after the system times.
    pub reference_ns: i64,
    /// The reference's advance across the reading (its uncertainty).
    pub window_ns: i64,
}

impl ClockReading {
    /// A reading from the reference read before (`reference_before_ns`) and after
    /// (`reference_after_ns`) the two system times.
    pub fn sandwiched(
        coarse_ns: i64,
        precise_ns: i64,
        reference_before_ns: i64,
        reference_after_ns: i64,
    ) -> Self {
        let window_ns = reference_after_ns - reference_before_ns;
        ClockReading {
            coarse_ns,
            precise_ns,
            reference_ns: reference_before_ns + window_ns / 2,
            window_ns,
        }
    }
}

/// The operating system under the step law.
pub trait StepOps {
    /// Read the three clocks: the reference, the coarse and the precise system time, the
    /// reference again ([`ClockReading::sandwiched`]).
    fn read(&mut self) -> ClockReading;
    /// Set the system time to `target_ns` (ns since the Unix epoch).
    fn set(&mut self, target_ns: i64) -> Result<(), String>;
}

/// A step is done once its residual (requested − realized) is within this.
pub const STEP_TOLERANCE_NS: i64 = 10_000;

/// At most this many sets per step (the first plus corrections; each blocks ~117 ms on
/// Windows, and a correction is needed only after a preempted set — the bench's worst case is
/// three preemptions in a row).
pub const MAX_STEP_ATTEMPTS: u32 = 4;

/// The learned read→set latency is clamped to ±this.
pub const MAX_LEAD_NS: i64 = 1_000_000;

/// One observation lowers the learned latency by at most this …
pub const LEAD_SLEW_NS: i64 = 5_000;

/// … and raises it by at most this: a preempted set (hundreds of µs, a few % of the calls) is an
/// outlier, and a run of them must not make the next steps overshoot — an overshoot is never
/// stepped back. A genuinely larger latency is still learned, a µs per observation.
pub const LEAD_RISE_NS: i64 = 1_000;

/// A reading whose reference reads are further apart than this was preempted: read again.
pub const READ_WINDOW_MAX_NS: i64 = 20_000;

/// At most this many readings for one; the tightest is used if none is within the window.
pub const READ_TRIES: u32 = 8;

/// A residual is only corrected up to this. A set lands late by its latency — a few µs, a
/// preempted one a few hundred (even for a step smaller than that) — so a larger residual means
/// the measurement is wrong (another writer stepped the clock during the call, a clock read
/// failed), and chasing it could move the wall by anything.
pub const MAX_CORRECTION_NS: i64 = 2_000_000;

/// The learned read→set latency (ns) of this clock, carried from step to step.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepLead {
    lead_ns: i64,
}

impl StepLead {
    pub fn lead_ns(&self) -> i64 {
        self.lead_ns
    }

    /// A lead that has learned `latency_ns` (tests, and a clock that knows its latency).
    pub fn seeded(latency_ns: i64) -> Self {
        StepLead {
            lead_ns: latency_ns,
        }
    }

    /// One set landed `latency_ns` after its read: move the learned latency half-way towards it,
    /// down by at most [`LEAD_SLEW_NS`], up by at most [`LEAD_RISE_NS`].
    fn observe(&mut self, latency_ns: i64) {
        let pull = ((latency_ns - self.lead_ns) / 2).clamp(-LEAD_SLEW_NS, LEAD_RISE_NS);
        self.lead_ns = (self.lead_ns + pull).clamp(-MAX_LEAD_NS, MAX_LEAD_NS);
    }
}

/// What one step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepOutcome {
    pub requested_ns: i64,
    /// The wall's measured move (`Δprecise − Δreference` summed over the sets).
    pub realized_ns: i64,
    /// Sets made (0 for a zero step).
    pub attempts: u32,
    /// `precise − coarse` at the first read: what a coarse-read step would have lost.
    pub coarse_lag_ns: i64,
    /// Why the law stopped short of the tolerance, if it did: a correction set that failed, or a
    /// residual beyond [`MAX_CORRECTION_NS`] (the wall HAS moved by `realized_ns` either way).
    pub stopped: Option<String>,
}

/// Read `ops` until the reading is not preempted (its window within [`READ_WINDOW_MAX_NS`]), at
/// most [`READ_TRIES`] times; else the tightest reading.
pub fn read_tight<O: StepOps>(ops: &mut O) -> ClockReading {
    let mut best = ops.read();
    for _ in 1..READ_TRIES {
        if best.window_ns <= READ_WINDOW_MAX_NS {
            break;
        }
        let r = ops.read();
        if r.window_ns < best.window_ns {
            best = r;
        }
    }
    best
}

impl StepOutcome {
    /// `requested − realized`: the phase error this step leaves for the phase lock.
    pub fn residual_ns(&self) -> i64 {
        self.requested_ns.wrapping_sub(self.realized_ns)
    }
}

/// Step the wall of `ops` by `requested_ns`.
///
/// `Err` only when nothing was set (the target is out of range, or the first set failed); once a
/// set has landed the step is reported as made (`realized_ns`), with [`StepOutcome::stopped`]
/// saying why it stopped short of the tolerance, if it did.
pub fn step_wall<O: StepOps>(
    ops: &mut O,
    lead: &mut StepLead,
    requested_ns: i64,
) -> Result<StepOutcome, String> {
    let mut out = StepOutcome {
        requested_ns,
        realized_ns: 0,
        attempts: 0,
        coarse_lag_ns: 0,
        stopped: None,
    };
    if requested_ns == 0 {
        return Ok(out);
    }
    let mut before = read_tight(ops);
    out.coarse_lag_ns = before.precise_ns - before.coarse_ns;
    loop {
        let remaining = requested_ns - out.realized_ns;
        let target = before
            .precise_ns
            .checked_add(remaining)
            .and_then(|t| t.checked_add(lead.lead_ns))
            .filter(|t| *t > 0)
            .ok_or_else(|| format!("a step of {requested_ns} ns would leave the valid time range"));
        let set = target.and_then(|t| ops.set(t));
        if let Err(e) = set {
            if out.attempts == 0 {
                return Err(e);
            }
            out.stopped = Some(format!("a correction set failed: {e}"));
            return Ok(out);
        }
        out.attempts += 1;
        let after = read_tight(ops);
        // What the set did to the wall: its whole move minus the time that passed meanwhile.
        let moved =
            (after.precise_ns - before.precise_ns) - (after.reference_ns - before.reference_ns);
        out.realized_ns += moved;
        let residual = requested_ns - out.realized_ns;
        // Beyond this the residual is not the set's latency: the measurement cannot be explained.
        if residual.abs() > MAX_CORRECTION_NS {
            // Another writer moved the clock during the call, or a clock read failed: never chase
            // it (it could move the wall by anything), and never learn a latency from it.
            out.stopped = Some(format!(
                "the measured move ({moved} ns) cannot be this set's: not corrected"
            ));
            return Ok(out);
        }
        // It aimed at `remaining + lead` ahead of the read and landed `moved` ahead of it: the
        // difference is how long after the read the kernel applied it.
        lead.observe(remaining + lead.lead_ns - moved);
        // Done within the tolerance; an overshoot is never stepped back (a forward step never runs
        // the wall backwards, a backward one never forwards).
        if residual.abs() <= STEP_TOLERANCE_NS || residual.signum() != requested_ns.signum() {
            return Ok(out);
        }
        if out.attempts >= MAX_STEP_ATTEMPTS {
            out.stopped = Some(format!(
                "still {residual} ns short after {} sets",
                out.attempts
            ));
            return Ok(out);
        }
        before = after;
    }
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
        /// Per read (in order), a preemption between the first reference read and the system
        /// times (0 = none; reads beyond the list are not preempted).
        read_gaps: Vec<i64>,
        reads: usize,
        /// Per set (in order): `Err` refuses it; `Ok(jump)` lets another writer move the wall by
        /// `jump` during the call (sets beyond the list succeed with no jump).
        set_script: Vec<Result<i64, String>>,
    }

    impl FakeOs {
        fn new(true_ns: i64, tick_ns: i64, latency_ns: i64) -> Self {
            FakeOs {
                true_ns,
                stepped_ns: 0,
                tick_ns,
                latencies: vec![latency_ns],
                sets: 0,
                read_gaps: Vec::new(),
                reads: 0,
                set_script: Vec::new(),
            }
        }
        fn wall(&self) -> i64 {
            self.true_ns + self.stepped_ns
        }
    }

    impl StepOps for FakeOs {
        fn read(&mut self) -> ClockReading {
            let gap = self.read_gaps.get(self.reads).copied().unwrap_or(0);
            self.reads += 1;
            let reference_before = self.true_ns;
            self.true_ns += gap + US / 2;
            let precise = self.wall();
            let coarse = precise - self.true_ns.rem_euclid(self.tick_ns);
            self.true_ns += US / 2;
            ClockReading::sandwiched(coarse, precise, reference_before, self.true_ns)
        }
        fn set(&mut self, target_ns: i64) -> Result<(), String> {
            let i = (self.sets as usize).min(self.latencies.len() - 1);
            let script = self.set_script.get(self.sets as usize).cloned();
            self.sets += 1;
            let jump = match script {
                Some(Err(e)) => return Err(e),
                Some(Ok(jump)) => jump,
                None => 0,
            };
            self.true_ns += self.latencies[i];
            self.stepped_ns = target_ns - self.true_ns + jump;
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
        let mut os = FakeOs::new(T0 + 250 * US - US / 2, 500 * US, 3 * US);
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
            lead.lead_ns().abs() <= 10 * US,
            "one preemption barely moves the learned latency: {}",
            lead.lead_ns()
        );
    }

    #[test]
    fn an_overshoot_is_never_stepped_back() {
        // A lead far above the real latency: the forward step overshoots, and the wall is not
        // stepped backwards to take it back (one set, the overshoot reported).
        let mut lead = StepLead::seeded(50 * US);
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        let out = step_wall(&mut os, &mut lead, 500 * US).unwrap();
        assert_eq!(out.attempts, 1, "{out:?}");
        // (The set lands its latency after the read's wall sample, half a µs before its end.)
        assert_eq!(out.realized_ns, 500 * US + 50 * US - 3 * US - US / 2);
        assert!(out.residual_ns() < 0);
        // The learned latency follows the real 3 µs once those are the majority of what it
        // remembers (one observation against a history is an outlier either way).
        for _ in 0..8 {
            os.true_ns += 20_000 * MS;
            step_wall(&mut os, &mut lead, 500 * US).unwrap();
        }
        assert!(
            lead.lead_ns() <= 5 * US,
            "learned towards 3 µs: {}",
            lead.lead_ns()
        );
    }

    #[test]
    fn a_late_set_of_a_backward_step_is_corrected_forward() {
        // −500 µs, the set preempted 300 µs: the wall moved −800 µs, further back than asked. The
        // fix steps it FORWARD — which never runs a wall backwards — so it is made.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.latencies = vec![300 * US, 3 * US];
        let out = step_wall(&mut os, &mut StepLead::default(), -500 * US).unwrap();
        assert_eq!(out.attempts, 2, "{out:?}");
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
        assert_eq!(out.stopped, None);
    }

    #[test]
    fn a_steady_large_set_latency_is_learned_within_three_sets() {
        // A box whose kernel applies every set 200 µs after the read: after three sets the steps
        // land in one set, exactly.
        let mut lead = StepLead::default();
        let mut os = FakeOs::new(T0, 500 * US, 200 * US);
        let mut outs = Vec::new();
        for k in 0..4 {
            os.true_ns += 20_000 * MS + 37 * US * k;
            outs.push(step_wall(&mut os, &mut lead, 500 * US).unwrap());
        }
        for out in &outs {
            assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
        }
        assert_eq!(outs[3].attempts, 1, "{outs:?}");
        assert!(outs[3].residual_ns().abs() <= 2 * US, "{outs:?}");
    }

    #[test]
    fn an_isolated_preempted_set_does_not_make_the_next_step_overshoot() {
        let mut lead = StepLead::default();
        let mut os = FakeOs::new(T0, 500 * US, 5 * US);
        for _ in 0..3 {
            os.true_ns += 20_000 * MS;
            step_wall(&mut os, &mut lead, 500 * US).unwrap();
        }
        // One set preempted 350 µs, then the steady 5 µs again.
        os.latencies = vec![350 * US, 5 * US];
        os.sets = 0;
        os.true_ns += 20_000 * MS;
        step_wall(&mut os, &mut lead, 500 * US).unwrap();
        os.latencies = vec![5 * US];
        os.true_ns += 20_000 * MS;
        let out = step_wall(&mut os, &mut lead, 500 * US).unwrap();
        assert_eq!(out.attempts, 1, "{out:?}");
        assert!(out.residual_ns().abs() <= 2 * US, "{out:?}");
    }

    #[test]
    fn a_preempted_after_read_is_read_again() {
        // The reading AFTER the set is the preempted one: taken alone it would mis-measure the
        // move by 300 µs.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.read_gaps = vec![0, 600 * US];
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(
            out.realized_ns, os.stepped_ns,
            "the measurement is the true move"
        );
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
    }

    #[test]
    fn a_set_cannot_land_early() {
        // A forward move beyond what the learned latency explains (another writer stepped the
        // clock +100 µs during a +500 µs step): an overshoot no set can make — not "corrected",
        // reported.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.set_script = vec![Ok(100 * US)];
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(out.attempts, 1, "{out:?}");
        assert!(out.stopped.is_some(), "{out:?}");
    }

    #[test]
    fn the_attempts_are_bounded() {
        // A latency that grows with every set can never be caught up with: the sets stop at the
        // bound, and the outcome says so.
        let mut os = FakeOs::new(T0, 500 * US, 400 * US);
        os.latencies = (1..=10).map(|k| 400 * US * k).collect();
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(out.attempts, MAX_STEP_ATTEMPTS);
        assert_eq!(os.sets, MAX_STEP_ATTEMPTS);
        assert!(out.stopped.is_some(), "{out:?}");
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
        let mut os = FakeOs::new(T0, 500 * US, 3 * US);
        os.set_script = vec![Err("NtSetSystemTime failed with NTSTATUS 0xC0000061".into())];
        let err = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap_err();
        assert!(err.contains("0xC0000061"));
        assert_eq!(os.stepped_ns, 0, "nothing moved");
    }

    #[test]
    fn a_failed_correction_keeps_the_move_already_made() {
        // The first set lands 300 µs short (preempted), the correction set is refused: the wall
        // HAS moved 200 µs, so the step is reported as made (D must move with it), with why it
        // stopped short — never an error that makes the caller leave D behind.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 300 * US);
        os.set_script = vec![
            Ok(0),
            Err("NtSetSystemTime failed with NTSTATUS 0xC0000061".into()),
        ];
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US)
            .expect("the wall already moved: the step is reported, not failed");
        assert_eq!(out.realized_ns, os.stepped_ns);
        assert_eq!(
            out.realized_ns,
            200 * US - US / 2,
            "short by the latency after the wall sample"
        );
        assert_eq!(out.attempts, 1, "one set landed");
        assert!(
            out.stopped
                .as_deref()
                .is_some_and(|s| s.contains("0xC0000061")),
            "{out:?}"
        );
    }

    #[test]
    fn a_preempted_read_is_read_again_and_never_mis_measures_the_step() {
        // The first reading is preempted 600 µs between its reference and wall reads: taken
        // alone it would place the wall 300 µs off the reference, and the law would "correct" a
        // step that was exact.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.read_gaps = vec![600 * US];
        let out = step_wall(&mut os, &mut StepLead::default(), 500 * US).unwrap();
        assert_eq!(
            out.realized_ns, os.stepped_ns,
            "the measurement is the true move"
        );
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
    }

    #[test]
    fn read_tight_takes_the_tightest_reading() {
        let mut os = FakeOs::new(T0, 500 * US, 3 * US);
        os.read_gaps = vec![900 * US, 30 * US, 700 * US];
        os.read_gaps.resize(READ_TRIES as usize, 400 * US);
        let r = read_tight(&mut os);
        assert_eq!(
            r.window_ns,
            30 * US + US,
            "the tightest of the preempted readings"
        );
        assert_eq!(os.reads, READ_TRIES as usize, "all of them tried");
        // A clean reading is taken at once.
        let mut os = FakeOs::new(T0, 500 * US, 3 * US);
        os.read_gaps = vec![900 * US];
        assert_eq!(read_tight(&mut os).window_ns, US);
        assert_eq!(os.reads, 2);
    }

    #[test]
    fn a_preempted_set_of_a_step_smaller_than_its_latency_is_still_corrected() {
        // A 30 µs step whose set is preempted 300 µs: the wall moved 270 µs BACKWARDS. That is a
        // set's latency, not a wrong measurement — corrected, never left as a 300 µs error.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.latencies = vec![300 * US, 3 * US];
        let out = step_wall(&mut os, &mut StepLead::default(), 30 * US).unwrap();
        assert_eq!(out.attempts, 2, "{out:?}");
        assert!(out.residual_ns().abs() <= STEP_TOLERANCE_NS, "{out:?}");
        assert_eq!(out.stopped, None);
    }

    #[test]
    fn a_measurement_the_step_cannot_explain_is_not_chased() {
        // Another writer steps the clock +3 ms during the set of a −500 µs step: the measured move
        // is +2.5 ms, a residual of −3 ms — beyond the step itself. Chasing it would move the wall
        // by an unverified amount: the law stops and says why.
        let mut os = FakeOs::new(T0 + 11 * US, 500 * US, 3 * US);
        os.set_script = vec![Ok(3 * MS)];
        let out = step_wall(&mut os, &mut StepLead::default(), -500 * US).unwrap();
        assert_eq!(out.attempts, 1, "{out:?}");
        assert!(out.stopped.is_some(), "{out:?}");
        assert_eq!(os.sets, 1);
    }
}
