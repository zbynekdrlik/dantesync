//! dantesync#119 (1.12) — the date authority in DAILY mode: readings all day, nothing announced;
//! at the nightly window ONE coordinated step of the error rounded to the 200 ms quantum (1.16),
//! either direction, the remainder carried; the emergency cap, never rounded; the micro mode
//! untouched. Closed-loop: the simulated fleet line drifts, is read
//! every 10 s, and moves by exactly the step at its instant.

use super::*;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
/// 2026-09-26T00:00:00Z.
const DAY0: i64 = 1_790_380_800 * S;
/// The master's PTP "now" when the test starts.
const PTP0: i64 = 1_000 * S;
/// Two 5 s leads.
const LEAD2: i64 = MICRO_LEAD_FACTOR * MIN_STEP_LEAD_NS;

/// A daily authority whose fleet wall reads `start_wall` at `PTP0`.
fn daily_authority(start_wall: i64, cfg: DailyConfig) -> DateAuthority {
    DateAuthority::new(start_wall - PTP0, PTP0, 0, 0).with_correction(CorrectionMode::Daily(cfg))
}

/// The closed loop: the fleet line's UTC error drifts at `ppm`, is read every 10 s (a reading is
/// skipped while `utc_down(t)`), and every announced step moves the line at its instant.
struct Loop {
    a: DateAuthority,
    ptp: i64,
    /// UTC − fleet line, ns, before the steps announced but not landed.
    error: i64,
    ppm: f64,
    /// Every announce: (its effective instant, its size, the fleet wall and the PTP time at the
    /// announce).
    steps: Vec<(i64, i64, i64, i64)>,
    events: Vec<DailyDecision>,
}

impl Loop {
    fn new(a: DateAuthority, error: i64, ppm: f64) -> Self {
        Loop {
            a,
            ptp: PTP0,
            error,
            ppm,
            steps: Vec::new(),
            events: Vec::new(),
        }
    }

    fn wall(&mut self) -> i64 {
        self.ptp + self.a.current_offset_ns(self.ptp)
    }

    /// Run for `secs`, ticking every second and reading every 10 s unless `utc_down(wall)`.
    fn run(&mut self, secs: i64, utc_down: impl Fn(i64) -> bool) {
        for _ in 0..secs {
            self.ptp += S;
            self.error += (self.ppm * 1_000.0) as i64;
            // A pending step whose instant has come moves the fleet line (the error drops by it).
            let before = self.a.current_offset_ns(self.ptp - S);
            let after = self.a.current_offset_ns(self.ptp);
            self.error -= after - before;
            let wall = self.wall();
            if self.ptp % (10 * S) == 0 && !utc_down(wall) {
                if let Some(ann) = self.a.on_utc_error(self.error, self.ptp) {
                    self.steps.push((
                        ann.effective_ptp_ns,
                        ann.date_offset_ns - after,
                        wall,
                        self.ptp,
                    ));
                }
            }
            if let Some(ann) = self.a.on_tick(self.ptp) {
                assert!(
                    ann.as_slew().is_none(),
                    "never a slew in daily mode: {ann:?}"
                );
                self.steps.push((
                    ann.effective_ptp_ns,
                    ann.date_offset_ns - after,
                    wall,
                    self.ptp,
                ));
            }
            if let Some(ev) = self.a.take_daily_event() {
                self.events.push(ev);
            }
        }
    }
}

fn at(h: i64, m: i64) -> i64 {
    DAY0 + (h * 3_600 + m * 60) * S
}

/// The unrounded error the `i`-th nightly step was decided on.
fn decided_error(events: &[DailyDecision], i: usize) -> i64 {
    let steps: Vec<i64> = events
        .iter()
        .filter_map(|e| match e {
            DailyDecision::Step { error_ns, .. } => Some(*error_ns),
            _ => None,
        })
        .collect();
    steps[i]
}

#[test]
fn a_daily_authority_announces_nothing_all_day_then_one_step_rounded_to_200_ms_119() {
    // 12:00 UTC, the fleet line 700 ms behind UTC, drifting +17.6 ppm (the rig).
    let a = daily_authority(at(12, 0), DailyConfig::default());
    let mut l = Loop::new(a, 700 * MS, 17.6);
    l.run(14 * 3_600 - 1, |_| false); // to 01:59:59
    assert!(l.steps.is_empty(), "nothing during the day: {:?}", l.steps);
    assert_eq!(l.a.seq(), 1);
    assert!(
        !l.a.micro().falling_behind(),
        "no falling-behind alarm in daily mode"
    );
    // ~1587.2 ms where it lands: stepped as 1600 ms (8 × 200 ms).
    let expected = 700 * MS + (17.6 * 1_000.0) as i64 * (14 * 3_600 + 10);
    l.run(2, |_| false);
    assert_eq!(l.steps.len(), 1, "one step at the window: {:?}", l.steps);
    let (eff, size, wall, announced_at) = l.steps[0];
    assert!(
        (wall - at(2, 0) - 86_400 * S).abs() <= S,
        "announced when the window opens"
    );
    assert_eq!(eff - announced_at, LEAD2, "two leads ahead");
    assert_eq!(
        size,
        1_600 * MS,
        "the error where it lands ({expected}) rounded to the 200 ms quantum"
    );
    let ann = l.a.announce();
    assert!(!ann.micro, "a nightly step is not a micro-correction");
    assert_eq!(ann.slew, None);
    assert_eq!(l.events.len(), 1, "{:?}", l.events);
    assert!(matches!(l.events[0], DailyDecision::Step { amount_ns, .. } if amount_ns == size));
    assert!(
        (decided_error(&l.events, 0) - expected).abs() < MS,
        "decided on the unrounded error: {:?} vs {expected}",
        l.events
    );
    assert_eq!(l.a.daily_last_step().map(|s| s.1), Some(size));
    // Landed: the remainder (~12.8 ms, the fleet now AHEAD of UTC) stays, within half a quantum.
    l.run(20, |_| false);
    assert!(
        (-100 * MS..0).contains(&l.error),
        "the remainder stays: {}",
        l.error
    );
    // … and the estimate knows it at once: the kept readings were compensated by the ROUNDED
    // step, not by the error it was decided on (review round 1).
    let est = l.a.micro().estimate(l.ptp).expect("an estimate").error_ns;
    assert!(
        (est - l.error).abs() < MS,
        "the estimate {est} vs the remainder {}",
        l.error
    );
    // The rest of the night and the whole next day: nothing, the error regrows.
    l.run(23 * 3_600 - 20, |_| false);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    assert!(
        l.error.abs() > 1_300 * MS,
        "a day's drift again: {}",
        l.error
    );
    l.run(3_600, |_| false);
    assert_eq!(l.steps.len(), 2, "the next night: {:?}", l.steps);
    // The next night measures the remainder again: a day at +17.6 ppm (1520.64 ms) minus the
    // ~12.8 ms the first step overshot is ~1507.9 ms, stepped as 1600 ms.
    let second = expected - 1_600 * MS + (17.6 * 1_000.0) as i64 * 86_400;
    assert_eq!(l.steps[1].1, 1_600 * MS, "{:?}", l.steps);
    assert!(
        (decided_error(&l.events, 1) - second).abs() < MS,
        "the remainder measured again: {:?} vs {second}",
        l.events
    );
    // Two nights' steps track two nights' drift to within half a quantum.
    l.run(20, |_| false);
    assert!(l.error.abs() <= 100 * MS, "residual {}", l.error);
}

#[test]
fn a_fleet_ahead_of_utc_is_stepped_back_at_night_never_slewed_by_day_119() {
    let a = daily_authority(at(12, 0), DailyConfig::default());
    let mut l = Loop::new(a, -300 * MS, -15.0);
    l.run(15 * 3_600, |_| false);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    // ~-1056.2 ms where it lands: a backward STEP of 5 × 200 ms.
    assert_eq!(l.steps[0].1, -1_000 * MS, "a backward STEP: {:?}", l.steps);
    assert_eq!(l.a.slew_in_progress(l.ptp), None);
}

#[test]
fn an_error_beyond_the_emergency_cap_is_stepped_at_once_119() {
    let a = daily_authority(at(12, 0), DailyConfig::default());
    // 1.5 s (far beyond the 100 ms micro cap) waits for the night …
    let mut l = Loop::new(a, 1_500 * MS, 0.0);
    l.run(600, |_| false);
    assert!(l.steps.is_empty(), "{:?}", l.steps);
    // … 6.12 s does not: two agreeing readings, then one step of the whole reading — NOT rounded
    // to the nightly quantum — lead ahead.
    l.error = 6_123_456_789;
    l.run(20, |_| false);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    assert!((l.steps[0].1 - 6_123_456_789).abs() < MS, "{:?}", l.steps);
    assert_ne!(
        l.steps[0].1 % (200 * MS),
        0,
        "an emergency is never rounded: {:?}",
        l.steps
    );
    assert_eq!(
        l.steps[0].0 - l.steps[0].3,
        MIN_STEP_LEAD_NS,
        "one lead ahead"
    );
    assert!(
        l.a.daily_last_step().is_none(),
        "an emergency is not the nightly step"
    );
    // Backward too.
    let a = daily_authority(at(12, 0), DailyConfig::new(DEFAULT_DAILY_STEP_TOD_S, 2_000));
    let mut l = Loop::new(a, -2_500 * MS, 0.0);
    l.run(30, |_| false);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    assert!(l.steps[0].1 < -2_400 * MS);
}

#[test]
fn a_master_booted_6_h_ahead_is_stepped_back_at_once_and_still_steps_the_next_night_119() {
    // Review round 2: the emergency (the case it exists for: a bad boot) steps the fleet wall
    // back across a night; the next window must still step, and nothing is reported missed.
    let a = daily_authority(at(4, 0), DailyConfig::default());
    let mut l = Loop::new(a, -6 * 3_600 * S, 17.6);
    l.run(30, |_| false);
    assert_eq!(l.steps.len(), 1, "the emergency: {:?}", l.steps);
    assert!(l.steps[0].1 < -6 * 3_600 * S + S);
    // From ~22:00 (fleet) to past the next 02:00.
    l.run(5 * 3_600, |_| false);
    assert_eq!(l.steps.len(), 2, "the next night stepped: {:?}", l.steps);
    // The emergency step (the whole reading) leaves the fleet wall off the loop's whole-second
    // grid: the window is judged to the second.
    let tod = (l.steps[1].2 - DAY0).rem_euclid(86_400 * S);
    assert!(
        (2 * 3_600 * S..2 * 3_600 * S + S).contains(&tod),
        "when the window opens: {tod}"
    );
    assert!(
        l.events
            .iter()
            .all(|e| matches!(e, DailyDecision::Step { .. })),
        "no missed / skipped night: {:?}",
        l.events
    );
}

#[test]
fn a_utc_outage_over_the_window_steps_when_utc_returns_inside_it_else_the_next_night_119() {
    // UTC down 01:55 – 02:12: the step is made at the first fresh estimate after 02:12.
    let a = daily_authority(at(12, 0), DailyConfig::default());
    let mut l = Loop::new(a, 0, 17.6);
    let down = |w: i64| {
        let tod = (w - DAY0).rem_euclid(86_400 * S);
        (115 * 60 * S..132 * 60 * S).contains(&tod)
    };
    l.run(15 * 3_600, down);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    let tod = (l.steps[0].2 - DAY0).rem_euclid(86_400 * S);
    // Review round 1: never on the first reading back — the level needs MICRO_MIN_READINGS fresh
    // readings (the sixth arrives 50 s after UTC is back), so one WAN outlier cannot set the
    // whole step.
    assert!(
        ((132 * 60 + 50) * S..150 * 60 * S).contains(&tod),
        "inside the window, once the estimate has fresh readings again: {}",
        tod / S
    );
    assert!(matches!(l.events[0], DailyDecision::Waiting { .. }));
    assert!(matches!(l.events[1], DailyDecision::Step { .. }));

    // UTC down 01:55 – 02:45: the night is skipped (loudly), the next night steps two days.
    let a = daily_authority(at(12, 0), DailyConfig::default());
    let mut l = Loop::new(a, 0, 17.6);
    let down = |w: i64| {
        let tod = (w - DAY0).rem_euclid(86_400 * S);
        w < DAY0 + 2 * 86_400 * S && (115 * 60 * S..165 * 60 * S).contains(&tod)
    };
    l.run(39 * 3_600, down);
    assert_eq!(l.steps.len(), 1, "{:?}", l.steps);
    // The error since 12:00 two days before, where the step lands (02:00:10): ~2407.9 ms,
    // stepped as 2400 ms.
    let expected = (17.6 * 1_000.0) as i64 * (38 * 3_600 + 10);
    assert_eq!(
        l.steps[0].1,
        2_400 * MS,
        "the next night steps both days: {:?} vs {expected}",
        l.steps
    );
    assert!(
        (decided_error(&l.events, 0) - expected).abs() < MS,
        "{:?} vs {expected}",
        l.events
    );
    let tod = (l.steps[0].2 - DAY0).rem_euclid(86_400 * S);
    assert_eq!(tod, 2 * 3_600 * S, "when the next window opens");
    assert!(matches!(l.events[0], DailyDecision::Waiting { .. }));
    assert_eq!(
        l.events[1],
        DailyDecision::Skipped {
            next_window_wall_ns: at(2, 0) + 2 * 86_400 * S
        }
    );
    assert!(matches!(l.events[2], DailyDecision::Step { .. }));
}

#[test]
fn the_next_window_is_published_on_the_fleet_wall_119() {
    let mut a = daily_authority(at(12, 0), DailyConfig::default());
    assert_eq!(
        a.correction_mode(),
        CorrectionMode::Daily(DailyConfig::default())
    );
    assert_eq!(
        a.daily_next_window_wall_ns(PTP0),
        Some(at(2, 0) + 86_400 * S)
    );
    assert_eq!(a.take_daily_event(), None);
    let micro = DateAuthority::new(at(12, 0) - PTP0, PTP0, 0, 0);
    assert_eq!(micro.correction_mode(), CorrectionMode::Micro);
    assert_eq!(micro.daily_next_window_wall_ns(PTP0), None);
}

#[test]
fn micro_mode_still_makes_micro_corrections_119() {
    let a = DateAuthority::new(at(12, 0) - PTP0, PTP0, 0, 0).with_correction(CorrectionMode::Micro);
    let mut l = Loop::new(a, 7 * MS, 17.6);
    l.run(600, |_| false);
    assert!(l.steps.len() > 5, "{:?}", l.steps);
    assert!(l.steps.iter().all(|s| s.1 > 0 && s.1 <= 500_000));
    assert!(l.events.is_empty());
}
