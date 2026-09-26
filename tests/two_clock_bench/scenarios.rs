//! The bench's checks and its scenarios (the harness above is the controller glue it mirrors).

use super::*;

fn check(sc: &Scenario, r: &RunResult) {
    let label = sc.label;
    let offline_scenario = !sc.master_ptp_offline.is_empty();
    let n = r.words.len();
    println!(
        "[{label}] max wall disagreement {} µs, master |UTC − wall| max {} ms, rebases {:?}, \
         {} replies refused as another time base",
        r.max_disagreement_ns / US,
        r.max_utc_error_ns / MS,
        r.rebases,
        r.refused_replies
    );

    // PHASE: all walls agree within 100 µs at every instant after the join.
    assert!(
        r.max_disagreement_ns < 100 * US,
        "[{label}] walls disagreed by {} µs",
        r.max_disagreement_ns / US
    );
    // … and while the fleet settles the double fault, within `SETTLING_BOUND_NS`.
    println!(
        "[{label}] max wall disagreement while settling the double fault {} µs",
        r.max_settling_disagreement_ns / US
    );
    assert!(
        r.max_settling_disagreement_ns < SETTLING_BOUND_NS,
        "[{label}] walls disagreed by {} µs while settling the double fault",
        r.max_settling_disagreement_ns / US
    );
    if sc.gm_change_in_master_outage {
        assert!(
            r.max_settling_disagreement_ns > 0,
            "[{label}] the settling window was actually measured"
        );
    }

    // RATE: effective rate == the grandmaster's rate, every box, every clean hour.
    let worst = r
        .rate_errors_ppm
        .iter()
        .fold(0.0f64, |a, &x| a.max(x.abs()));
    println!(
        "[{label}] worst hourly rate error vs the GM: {worst:.5} ppm over {} box-hours",
        r.rate_errors_ppm.len()
    );
    assert!(
        worst < 0.01,
        "[{label}] a box's rate left the PTP rate by {worst} ppm"
    );

    // DATE: every follower joined once at boot, then took EXACTLY the announced steps (same seq,
    // same size), all coordinated — zero late, zero uncoordinated. The master took them too,
    // except (in the master-offline scenario) one it had to drop while running its local path;
    // its own re-alignment to the fleet line afterwards is a Join.
    assert!(
        !r.announced.is_empty() || !r.announced_slews.is_empty(),
        "[{label}] the authority never announced a date change"
    );
    // #119: THE FLEET DATE NEVER STEPS BACKWARDS. Every announced step is forward, every backward
    // correction is a slew, and no box ever applied a backward coordinated (or late) step.
    assert!(
        r.announced.iter().all(|a| a.1 > 0),
        "[{label}] a backward step was announced: {:?}",
        r.announced
    );
    assert!(
        r.announced_slews.iter().all(|a| a.1 < 0),
        "[{label}] a slew is only for a backward correction: {:?}",
        r.announced_slews
    );
    for (i, steps) in r.steps.iter().enumerate() {
        assert!(
            steps
                .iter()
                .filter(|s| s.2 != StepKind::Join)
                .all(|s| s.1 > 0 || (sc.gm_change_in_master_outage && s.1.abs() < MS)),
            "[{label}] box {i} stepped backwards: {steps:?}"
        );
    }
    println!(
        "[{label}] {} slews, relative phase max {} µs ({} µs while slewing, {} windows)",
        r.announced_slews.len(),
        r.max_relative_phase_ns / US,
        r.max_relative_phase_in_slew_ns / US,
        r.slew_windows
    );
    if !offline_scenario {
        assert_eq!(
            r.wall_went_back, 0,
            "[{label}] a wall ran backwards after the join"
        );
    }
    let master_coord: Vec<(u32, i64)> = r.steps[0]
        .iter()
        .filter(|s| s.2 == StepKind::Coordinated)
        .map(|s| (s.0, s.1))
        .collect();
    assert!(
        r.steps[0]
            .iter()
            .all(|s| s.2 == StepKind::Coordinated || (offline_scenario && s.2 == StepKind::Join)),
        "[{label}] the master made a step that is neither coordinated nor its re-alignment: {:?}",
        r.steps[0]
    );
    assert!(
        master_coord.iter().all(|c| r.announced.contains(c)),
        "[{label}] the master stepped something it never announced"
    );
    if !offline_scenario {
        assert_eq!(
            master_coord, r.announced,
            "[{label}] the master skipped an announce"
        );
    }
    for (i, steps) in r.steps.iter().enumerate().skip(1) {
        let (lates, steps): (Vec<&Step>, Vec<&Step>) = steps
            .iter()
            .partition(|s| sc.gm_change_in_master_outage && s.2 == StepKind::Late);
        assert!(
            lates.len() <= 1 && lates.iter().all(|s| s.1.abs() < MS),
            "[{label}] box {i}: the double-fault re-basing may cost one late step < 1 ms: {lates:?}"
        );
        let (joins, rest): (Vec<&Step>, Vec<&Step>) =
            steps.into_iter().partition(|s| s.2 == StepKind::Join);
        assert!(
            joins.len() <= 1,
            "[{label}] box {i} joined {} times",
            joins.len()
        );
        assert!(
            joins.iter().all(|s| s.3 < 10e9),
            "[{label}] box {i} joined late: {joins:?}"
        );
        assert!(
            rest.iter().all(|s| s.2 == StepKind::Coordinated),
            "[{label}] box {i} made an uncoordinated step: {rest:?}"
        );
        let got: Vec<(u32, i64)> = rest.iter().map(|s| (s.0, s.1)).collect();
        assert_eq!(
            got, r.announced,
            "[{label}] box {i} did not take exactly the announced steps"
        );
    }
    // SIMULTANEITY: every announced step landed on every box that applied it within 100 µs.
    let mut worst_spread = 0.0f64;
    for (seq, _) in &r.announced {
        let landings: Vec<f64> = r
            .steps
            .iter()
            .flat_map(|steps| {
                steps
                    .iter()
                    .filter(|s| s.2 == StepKind::Coordinated && s.0 == *seq)
                    .map(|s| s.3)
            })
            .collect();
        let spread = landings.iter().cloned().fold(f64::MIN, f64::max)
            - landings.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            spread < 100_000.0,
            "[{label}] step seq {seq} landed across {spread} ns"
        );
        worst_spread = worst_spread.max(spread);
    }
    println!(
        "[{label}] {} coordinated steps, worst landing spread {:.1} µs, {} in-flight samples",
        r.announced.len(),
        worst_spread / 1_000.0,
        r.in_flight_samples
    );
    let late_allowed = if sc.gm_change_in_master_outage { 1 } else { 0 };
    assert!(
        r.late_steps.iter().all(|&l| l <= late_allowed),
        "[{label}] late steps {:?}",
        r.late_steps
    );
    // Every box re-anchored exactly twice — at the grandmaster change and at its reboot — and
    // never stepped for either; the replies it saw in another time base meanwhile were refused.
    assert!(
        r.rebases.iter().all(|&x| x == 2),
        "[{label}] rebases {:?}",
        r.rebases
    );
    assert!(
        r.refused_replies > 0,
        "[{label}] the master-lags-the-fleet windows must actually be exercised"
    );

    // UTC: the master holds the date within the bound (+ the drift accrued over the 5 s lead and
    // the 2-reading confirmation, + NTP noise).
    assert!(
        r.max_utc_error_ns < DEFAULT_STEP_BOUND_NS + 2 * MS,
        "[{label}] master drifted {} ms from UTC",
        r.max_utc_error_ns / MS
    );
    // … and so does the FLEET (read on a follower), whatever happens to the master's own wall.
    assert!(
        r.max_fleet_utc_error_ns < DEFAULT_STEP_BOUND_NS + 2 * MS,
        "[{label}] the fleet line drifted {} ms from UTC",
        r.max_fleet_utc_error_ns / MS
    );
    assert_eq!(n, 6);
}

#[test]
fn two_clock_bench_rate_is_ptp_phase_agrees_and_every_date_step_is_coordinated_117_88() {
    // Two forward-only UTC scenarios (#119: a backward correction is a slew, which adds a rate
    // term by design, so the bit-identity pair is two runs whose date only STEPS).
    let sc_plus = Scenario::plain("UTC +8 ppm vs GM", 8.0, false);
    let sc_fast = Scenario::plain("UTC +20 ppm vs GM", 20.0, false);
    let plus = run(&sc_plus);
    check(&sc_plus, &plus);
    let fast = run(&sc_fast);
    check(&sc_fast, &fast);

    // THE DECOUPLING STATEMENT, for the frequency LAW: the two runs share every PTP input and
    // differ ONLY in UTC (so in the number, size and timing of the date steps). The frequency
    // command of every box is bit-identical between them: NTP contributes nothing to the rate.
    for i in 0..plus.words.len() {
        assert_eq!(
            plus.words[i], fast.words[i],
            "box {i}: the frequency word depends on UTC — NTP leaked into the rate path"
        );
    }
    // … while the date genuinely differed (the runs are not trivially identical).
    assert_ne!(plus.announced.len(), fast.announced.len());
    assert!(plus.announced_slews.is_empty() && fast.announced_slews.is_empty());
    assert!(plus
        .announced
        .iter()
        .chain(&fast.announced)
        .all(|a| a.1 > 0));
}

#[test]
fn a_fleet_ahead_of_utc_slews_back_never_steps_back_and_keeps_its_relative_phase_119() {
    // UTC runs −15 ppm against the grandmaster: the fleet date runs AHEAD, so every correction
    // is backwards (the camera-box#1372 case). Every one must be a coordinated slew: no backward
    // step anywhere, no wall ever running back, the fleet within 50 µs of relative phase through
    // every slew, and the phase lock's law untouched.
    let sc = Scenario::plain("UTC -15 ppm vs GM: backward corrections slew", -15.0, false);
    let r = run(&sc);
    check(&sc, &r);
    assert!(
        r.announced_slews.len() >= 10,
        "a day at -15 ppm needs many backward corrections: {:?}",
        r.announced_slews
    );
    assert!(r.announced.is_empty(), "no step at all: {:?}", r.announced);
    assert_eq!(r.wall_went_back, 0);
    assert!(r.slew_windows > 10_000, "the fleet actually slewed");
    assert!(
        r.max_relative_phase_in_slew_ns <= 50 * US,
        "relative phase {} µs while slewing",
        r.max_relative_phase_in_slew_ns / US
    );
    assert!(
        r.max_relative_phase_ns <= 50 * US,
        "relative phase {} µs",
        r.max_relative_phase_ns / US
    );
    for (i, steps) in r.steps.iter().enumerate() {
        assert!(
            steps.iter().all(|s| s.2 == StepKind::Join && s.3 < 10e9),
            "box {i}: only its boot join, never a date step: {steps:?}"
        );
    }
    // The law: the slew is decoupled from the phase lock, so its words match a run whose date
    // only steps up to numerical noise (the de-slewed schedule vs the integrated rate term).
    let plus = run(&Scenario::plain("UTC +8 ppm vs GM", 8.0, false));
    let worst = r
        .words
        .iter()
        .zip(&plus.words)
        .flat_map(|(a, b)| a.iter().zip(b).map(|(x, y)| (x - y).abs()))
        .fold(0.0f64, f64::max);
    println!("[slew] worst word difference vs the forward-only run: {worst:.6} ppm");
    assert!(
        worst < 0.001,
        "the slew leaked into the PTP law: {worst} ppm"
    );
}

#[test]
fn a_slew_is_extended_and_runs_through_a_grandmaster_change_and_reboot_119() {
    // UTC (the upstream) steps back by 80 ms one minute before the grandmaster changes, again
    // 100 s later (the first slew is still running: the authority EXTENDS it), and one minute
    // before the grandmaster reboots. Every box must keep slewing together through the re-anchor
    // of both grandmaster events: no backward step, no wall ever running back, the relative
    // phase within 50 µs, and the fleet back within the bound of UTC afterwards.
    let mut sc = Scenario::plain(
        "UTC -15 ppm + UTC jumps back around the GM change and reboot",
        -15.0,
        true,
    );
    sc.utc_jumps = vec![
        (GM_CHANGE_AT_WINDOW - 120, -80 * MS),
        (GM_CHANGE_AT_WINDOW + 80, -80 * MS),
        (GM_REBOOT_AT_WINDOW - 120, -80 * MS),
    ];
    let r = run(&sc);
    check(&sc, &r);
    println!(
        "[jumps] {} slews, {} extensions, {} GM events while slewing, {} master catch-ups",
        r.announced_slews.len(),
        r.extensions,
        r.gm_events_in_slew,
        r.master_catch_ups
    );
    assert!(r.extensions >= 1, "the running slew was extended");
    assert_eq!(
        r.gm_events_in_slew, 2,
        "both grandmaster events fell inside a slew"
    );
    assert!(r.announced.is_empty(), "no step at all: {:?}", r.announced);
    assert_eq!(r.wall_went_back, 0);
    assert!(
        r.max_relative_phase_in_slew_ns <= 50 * US,
        "relative phase {} µs while slewing",
        r.max_relative_phase_in_slew_ns / US
    );
}

#[test]
fn with_the_controllers_post_step_grace_the_envelopes_still_hold_117_88() {
    // The controller drops 2 s of PTP windows after every step (the step transient), holding the
    // word. Those windows fall at UTC-dependent times, so the words are no longer bit-identical
    // across UTC scenarios — the hold carries no NTP value, it only delays the next update. What
    // must survive is every envelope: rate = the GM's, walls within 100 µs, only coordinated
    // steps, no step at a grandmaster change or reboot.
    for sc in [
        Scenario::plain("UTC +8 ppm vs GM, with grace", 8.0, true),
        Scenario::plain("UTC -15 ppm vs GM, with grace", -15.0, true),
    ] {
        check(&sc, &run(&sc));
    }
}

#[test]
fn a_multi_second_first_step_and_a_master_only_ptp_outage_stay_coordinated_117_88() {
    // The master boots 3 s behind UTC: its first announce is a 3 s step (a boot-time NTP failure,
    // an upstream jump). Later ONLY the master loses PTP for 10 minutes and runs its local NTP
    // date path. Neither may reach a follower as an uncoordinated (late) step, and the fleet must
    // stay on one line throughout.
    let sc = Scenario {
        label: "3 s first step + master-only PTP outage, with grace",
        utc_vs_gm_ppm: 8.0,
        grace: true,
        master_boot_err_ns: -3 * S,
        master_ptp_offline: vec![(6 * 3600 * 2, 6 * 3600 * 2 + 1_200)],
        gm_change_in_master_outage: false,
        utc_jumps: Vec::new(),
    };
    let r = run(&sc);
    check(&sc, &r);
    assert!(
        r.announced.iter().any(|a| a.1.abs() > 2 * S),
        "the multi-second first step was actually exercised"
    );
}

#[test]
fn a_long_master_outage_and_a_grandmaster_change_during_one_keep_the_fleet_on_utc_117_88() {
    // ONLY the master loses PTP for 3 hours (the fleet line would drift ~86 ms from UTC at +8 ppm
    // if its date froze), and later for 30 minutes that END 10 s after the grandmaster changed —
    // the master comes back on the new grandmaster while its own wall is off the fleet line. The
    // fleet must stay within the bound of UTC, coordinated, and never take the master's own
    // offset as a step.
    let sc = Scenario {
        label: "3 h master outage + GM change at the end of a 30 min one, with grace",
        utc_vs_gm_ppm: 8.0,
        grace: true,
        master_boot_err_ns: 0,
        master_ptp_offline: vec![
            (2 * 3600 * 2, 5 * 3600 * 2),
            (GM_CHANGE_AT_WINDOW - 3_600, GM_CHANGE_AT_WINDOW + 20),
        ],
        gm_change_in_master_outage: true,
        utc_jumps: Vec::new(),
    };
    let r = run(&sc);
    check(&sc, &r);
}
