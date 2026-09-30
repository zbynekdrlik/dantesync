//! dantesync#126 — the date authority across a restart of the NTP master (saved, restored: the
//! same `D`, seq and change in flight) and its coordinated step on request. Closed-loop where it
//! matters: the simulated fleet line drifts, is read every 10 s, and moves by exactly the step at
//! its instant.

use super::*;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
/// 2026-09-26T00:00:00Z.
const DAY0: i64 = 1_790_380_800 * S;
/// The master's PTP "now" when a test starts.
const PTP0: i64 = 1_000 * S;
/// Two 5 s leads.
const LEAD2: i64 = MICRO_LEAD_FACTOR * MIN_STEP_LEAD_NS;

fn at(h: i64, m: i64) -> i64 {
    DAY0 + (h * 3_600 + m * 60) * S
}

fn daily() -> CorrectionMode {
    CorrectionMode::Daily(DailyConfig::default())
}

/// The closed loop: the fleet line's UTC error drifts at `ppm`, is read every 10 s, and every
/// announced step moves the line at its instant.
struct Loop {
    a: DateAuthority,
    ptp: i64,
    /// UTC − fleet line, ns.
    error: i64,
    ppm: f64,
    /// Every announce the authority made by itself: (its instant, its size).
    steps: Vec<(i64, i64)>,
    events: Vec<DailyDecision>,
}

impl Loop {
    fn new(a: DateAuthority, ptp: i64, error: i64, ppm: f64) -> Self {
        Loop {
            a,
            ptp,
            error,
            ppm,
            steps: Vec::new(),
            events: Vec::new(),
        }
    }

    fn run(&mut self, secs: i64) {
        for _ in 0..secs {
            self.ptp += S;
            self.error += (self.ppm * 1_000.0) as i64;
            let before = self.a.current_offset_ns(self.ptp - S);
            let after = self.a.current_offset_ns(self.ptp);
            self.error -= after - before;
            if self.ptp % (10 * S) == 0 {
                if let Some(ann) = self.a.on_utc_error(self.error, self.ptp) {
                    self.steps
                        .push((ann.effective_ptp_ns, ann.date_offset_ns - after));
                }
            }
            if let Some(ann) = self.a.on_tick(self.ptp) {
                self.steps
                    .push((ann.effective_ptp_ns, ann.date_offset_ns - after));
            }
            if let Some(ev) = self.a.take_daily_event() {
                self.events.push(ev);
            }
        }
    }
}

// ---- the saved state -------------------------------------------------------------------------

#[test]
fn a_restored_authority_publishes_the_same_d_seq_and_announce_126() {
    // An authority some way into its life: re-based once (seq 2), nothing in flight.
    let mut a = DateAuthority::new(7 * S, PTP0, 0, 0);
    a.rebase(9 * S + 123, PTP0 + 60 * S);
    assert_eq!(a.seq(), 2);
    let now = PTP0 + 600 * S;
    let saved = a.persisted(now);
    assert_eq!(saved.seq, 2);
    assert_eq!(saved.d_ns, 9 * S + 123);
    assert_eq!(saved.pending, None);
    assert_eq!(saved.slew, None);

    // The master comes back two minutes later.
    let later = now + 120 * S;
    let r = DateAuthority::restore(&saved, later, 0, 0);
    assert_eq!(
        r.seq(),
        2,
        "the session continues (a follower sees no new session)"
    );
    assert_eq!(
        r.announce(),
        a.announce(),
        "byte-for-byte the same announce"
    );
    assert_eq!(r.in_effect_ns(later), a.in_effect_ns(later));
    assert_eq!(
        r.persisted(later),
        saved,
        "saving it again gives the same record"
    );
}

#[test]
fn a_restart_inside_a_steps_lead_keeps_the_step_and_one_after_it_has_it_landed_126() {
    // An abnormal +300 ms reading (micro mode: beyond 2 x the bound) confirmed twice: a coordinated
    // step is pending, 5 s ahead.
    let mut a = DateAuthority::new(7 * S, PTP0, 0, 0);
    assert_eq!(a.on_utc_error(300 * MS, PTP0 + 10 * S), None);
    let ann = a
        .on_utc_error(300 * MS, PTP0 + 20 * S)
        .expect("the step is announced");
    let land = ann.effective_ptp_ns;
    let saved = a.persisted(PTP0 + 20 * S);
    assert_eq!(saved.pending, Some((7 * S + 300 * MS, land)));
    assert_eq!(saved.d_ns, 7 * S, "the step has not landed");

    // Back before the instant: the SAME step is still pending (the fleet scheduled it).
    let early = land - 2 * S;
    let r = DateAuthority::restore(&saved, early, 0, 0);
    assert_eq!(r.announce(), ann);
    assert_eq!(r.pending_step_ns(early), Some(300 * MS));
    assert_eq!(r.in_effect_ns(early), 7 * S);
    assert_eq!(
        r.in_effect_ns(land),
        7 * S + 300 * MS,
        "it lands at its instant"
    );

    // Back after it: the step is in effect (the fleet stepped at the instant), same seq.
    let late = land + 30 * S;
    let mut r = DateAuthority::restore(&saved, late, 0, 0);
    assert_eq!(r.seq(), ann.seq);
    assert_eq!(r.pending_step_ns(late), None);
    assert_eq!(r.current_offset_ns(late), 7 * S + 300 * MS);
    assert_eq!(
        r.announce().effective_ptp_ns,
        land,
        "in effect since its instant"
    );
    // The record taken after the instant already has it landed.
    assert_eq!(a.persisted(late).d_ns, 7 * S + 300 * MS);
    assert_eq!(a.persisted(late).pending, None);
}

#[test]
fn a_restore_during_a_slew_continues_its_schedule_126() {
    let slew = DateSlew {
        from_ns: 7 * S,
        to_ns: 7 * S - 400_000,
        start_ptp_ns: PTP0 + 10 * S,
        ppm: 100,
    };
    let saved = AuthorityState {
        d_ns: 7 * S,
        since_ptp_ns: PTP0,
        seq: 4,
        pending: None,
        slew: Some(slew),
        micro: true,
        daily_last_step: None,
    };
    let mid = slew.start_ptp_ns + S; // 100 µs paid
    let r = DateAuthority::restore(&saved, mid, 0, 0);
    assert_eq!(r.slew_in_progress(mid), Some(slew));
    assert_eq!(r.in_effect_ns(mid), 7 * S - 100_000);
    let ann = r.announce();
    assert_eq!(ann.as_slew(), Some(slew));
    assert!(ann.micro, "the micro kind stays with the seq");
    assert_eq!(ann.seq, 4);
    // Restored after its end: the end offset is in effect.
    let mut done = DateAuthority::restore(&saved, slew.end_ptp_ns() + S, 0, 0);
    assert_eq!(
        done.current_offset_ns(slew.end_ptp_ns() + S),
        7 * S - 400_000
    );
    assert_eq!(done.slew_in_progress(slew.end_ptp_ns() + S), None);
    // The pure record says the same at every instant.
    for t in [
        PTP0,
        slew.start_ptp_ns,
        mid,
        slew.end_ptp_ns(),
        slew.end_ptp_ns() + S,
    ] {
        assert_eq!(saved.d_in_effect_at(t), slew.offset_at(t), "at {t}");
    }
}

#[test]
fn a_restored_daily_authority_keeps_its_last_nightly_step_and_the_night_it_handled_126() {
    // The nightly step at 02:00, 700 ms of error.
    let a = DateAuthority::new(at(1, 50) - PTP0, PTP0, 0, 0).with_correction(daily());
    let mut l = Loop::new(a, PTP0, 700 * MS, 17.6);
    l.run(15 * 60); // to 02:05
    assert_eq!(l.steps.len(), 1, "the night's step: {:?}", l.steps);
    let saved = l.a.persisted(l.ptp);
    let last = saved.daily_last_step.expect("the nightly step is saved");
    assert_eq!(l.a.daily_last_step(), Some(last));

    // The master restarts at 02:05, inside the window: the night is not decided again.
    let restored = DateAuthority::restore(&saved, l.ptp, 0, 0)
        .with_correction(daily())
        .with_daily_last_step(saved.daily_last_step);
    assert_eq!(restored.daily_last_step(), Some(last), "reported again");
    let mut r = Loop::new(restored, l.ptp, l.error, 17.6);
    r.run(40 * 60); // to 02:45, past the window
    assert!(r.steps.is_empty(), "{:?}", r.steps);
    assert!(
        r.events.is_empty(),
        "the night is handled — not even a 'not needed': {:?}",
        r.events
    );
    assert_eq!(r.a.seq(), saved.seq);
    assert_eq!(
        r.a.daily_next_window_wall_ns(r.ptp),
        Some(at(2, 0) + 86_400 * S),
        "the next window is tomorrow's"
    );

    // Without the saved nightly step the restored authority judges the same night AGAIN (here a
    // second step of the few ms drifted since the first): the saved step is what prevents it.
    let bare = DateAuthority::restore(&saved, l.ptp, 0, 0).with_correction(daily());
    let mut b = Loop::new(bare, l.ptp, l.error, 17.6);
    b.run(40 * 60);
    assert!(
        b.events
            .iter()
            .any(|e| matches!(e, DailyDecision::Step { .. } | DailyDecision::NoStep { .. })),
        "a second decision of the same night: {:?}",
        b.events
    );
}

// ---- the coordinated step on request ---------------------------------------------------------

/// A daily authority at 12:00 whose fleet line is `error` behind UTC, read for half an hour (the
/// robust line's drift needs its one-minute bins: with a few minutes the level lags the ramp).
fn read_for_half_an_hour(error: i64) -> Loop {
    let a = DateAuthority::new(at(12, 0) - PTP0, PTP0, 0, 0).with_correction(daily());
    let mut l = Loop::new(a, PTP0, error, 17.6);
    l.run(30 * 60);
    assert!(l.steps.is_empty(), "nothing by day: {:?}", l.steps);
    l
}

#[test]
fn a_step_on_request_is_one_coordinated_step_of_the_current_error_two_leads_ahead_126() {
    let mut l = read_for_half_an_hour(300 * MS);
    let now = l.ptp;
    let before = l.a.current_offset_ns(now);
    let ann = l.a.step_now(now).expect("the step is announced");
    assert_eq!(
        ann.effective_ptp_ns,
        now + LEAD2,
        "two leads ahead, like the night"
    );
    assert_eq!(ann.seq, 2);
    assert!(!ann.micro);
    assert_eq!(ann.slew, None, "a forward step, never a slew");
    let size = ann.date_offset_ns - before;
    let expected = l.error + (17.6 * 1_000.0 * 10.0) as i64; // the error where it lands
    assert!(
        (size - expected).abs() < MS,
        "the whole current error: {size} vs {expected}"
    );
    assert_eq!(l.a.pending_step_ns(now), Some(size));

    // It lands at its instant, and the readings are compensated at once: the rest of the day
    // announces nothing more.
    l.run(4 * 3_600);
    assert!(l.steps.is_empty(), "{:?}", l.steps);
    assert_eq!(l.a.current_offset_ns(l.ptp), before + size);
    assert!(
        (l.error - (17.6 * 1_000.0) as i64 * (4 * 3_600 - 10)).abs() < MS,
        "the error restarts from ~0 at the landing: {}",
        l.error
    );
    assert_eq!(
        l.a.daily_last_step(),
        None,
        "a step on request is not a nightly step"
    );
}

#[test]
fn a_step_on_request_is_refused_while_a_change_is_in_flight_126() {
    let mut l = read_for_half_an_hour(300 * MS);
    let now = l.ptp;
    l.a.step_now(now).expect("the first step");
    assert_eq!(l.a.step_now(now + S), Err(StepRefused::ChangeInFlight));
    // Once it has landed a new request is judged again.
    l.run(15);
    assert_ne!(l.a.step_now(l.ptp), Err(StepRefused::ChangeInFlight));
}

#[test]
fn a_step_on_request_needs_a_settled_utc_estimate_126() {
    let mut a = DateAuthority::new(at(12, 0) - PTP0, PTP0, 0, 0).with_correction(daily());
    assert_eq!(a.step_now(PTP0), Err(StepRefused::NoEstimate));
    // Five readings are one short of the minimum.
    let mut l = Loop::new(a, PTP0, 300 * MS, 0.0);
    l.run(50);
    assert_eq!(l.a.step_now(l.ptp), Err(StepRefused::NoEstimate));
    l.run(10);
    assert!(l.a.step_now(l.ptp).is_ok(), "the sixth reading settles it");
}

#[test]
fn a_step_on_request_never_steps_the_fleet_back_126() {
    let mut l = read_for_half_an_hour(-300 * MS);
    let now = l.ptp;
    let expected = l.error + (17.6 * 1_000.0 * 10.0) as i64; // still ahead where it would land
    match l.a.step_now(now) {
        Err(StepRefused::NotBehind { error_ns }) => {
            assert!((error_ns - expected).abs() < MS, "{error_ns} vs {expected}")
        }
        other => panic!("a fleet ahead of UTC is never stepped back on request: {other:?}"),
    }
    assert_eq!(l.a.seq(), 1, "nothing announced");
    assert_eq!(l.a.pending_step_ns(now), None);
}
