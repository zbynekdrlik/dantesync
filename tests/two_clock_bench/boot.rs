//! dantesync#134 — fast boot sync on two clocks: the fleet date placed at the FIRST PTP window,
//! one step per box, and no second step when the phase lock engages ~30-40 s later.
//!
//! The NTP master (its saved fleet D restored, its RTC 3.75 s behind: strih-pp 25.10.2026) and a
//! follower (40 ms off, a +70 ppm oscillator) boot against one grandmaster. Each box's wall ticks
//! at its oscillator plus the word, each window holds 4 PTP samples (`wall_rx − gm_tx` with the
//! box's path delay and the software-timestamp noise) and the core gets their median, as the
//! controller does. Before the lock the RATE servo owns the word; it is stood in by the shape
//! strih-snv logged on 24.10.2026 (the word walking 0 → −19.5 ppm over 40 s toward a −21 ppm
//! oscillator, still 7 µs/s off at 30 s): `−osc·(1 − e^(−t/20 s))`, and it LOCKS once that word
//! is within [`HANDOVER_PPM`] of the oscillator (the real servo locks on |rate| < 5 ppm for 5
//! windows): 37 s for −21 ppm, as on strih-snv. The controller's 2 s post-step grace drops 4
//! windows and holds the word.
//!
//! Proven:
//!
//! 1. Each box steps ONCE, right after its early anchor (three agreeing windows): the master in
//!    that iteration onto its saved fleet D, the follower at its next poll onto the published D, and no window after it re-anchors (no `Realigned`, so no
//!    second step) — although the +70 ppm follower drifted past `REANCHOR_ON_ENGAGE_NS` before
//!    its lock (the case the boot hold exists for).
//! 2. From 400 s after its lock each box is within 50 µs of the fleet line, and the two walls
//!    within 100 µs of each other (their path delays differ by 33 µs).
//! 3. The pull that retires the acquisition drift keeps the applied word within its bound
//!    (`ERROR_CLAMP_US / FSTEP_PULL_TAU_S` + the PI) and never reads as a frequency step.

use super::*;
use dantesync::ptp_phase_lock::{
    ANCHOR_CONFIRM_WINDOWS, ERROR_CLAMP_US, FSTEP_PULL_TAU_S, REANCHOR_ON_ENGAGE_NS,
};

/// The fleet line the saved state carries: `wall = gm + FLEET_D`.
const FLEET_D: i64 = 1_789_000_000 * S;
const LOCK_AFTER_MIN_S: f64 = 30.0;
/// The rate servo stand-in locks once its word is this close to the oscillator (ppm).
const HANDOVER_PPM: f64 = 3.0;
/// The stand-in's time constant (s).
const SERVO_TAU_S: f64 = 20.0;

/// The rate servo stand-in's word at window `w`.
fn servo_word(osc_ppm: f64, w: u64) -> f64 {
    let t = (w + 1) as f64 * WINDOW_S;
    -osc_ppm * (1.0 - (-t / SERVO_TAU_S).exp())
}

/// The window the stand-in locks at.
fn lock_window(osc_ppm: f64) -> u64 {
    (0..)
        .find(|&w| (servo_word(osc_ppm, w) + osc_ppm).abs() < HANDOVER_PPM)
        .expect("converges")
}

struct BootBox {
    label: &'static str,
    osc_ppm: f64,
    delay_ns: f64,
    /// The wall's error vs the fleet line at boot (ns).
    boot_off_ns: i64,
    /// Windows from the anchor to the date layer's step: 0 for the master (it restores and
    /// re-aligns in the anchoring iteration), 2 for a follower (its next 1 s authority poll).
    join_lag_windows: u64,
    seed: u64,
}

struct BootRun {
    steps: Vec<(u64, i64)>,
    anchor_events_after_first: u32,
    drift_at_lock_ns: f64,
    /// Per window after the lock: the TRUE phase error vs the fleet line (ns): the wall minus the
    /// grandmaster minus `FLEET_D`, plus the box's path delay (each wall sits its own latency
    /// behind the line, by design).
    line_err_ns: Vec<f64>,
    /// Per window: `wall − gm` (ns), for the cross-box comparison.
    wall_minus_gm: Vec<i64>,
    /// The largest |applied word − truth| after the lock (ppm).
    worst_word_err_ppm: f64,
    freq_steps: u32,
}

fn run_boot(b: &BootBox, windows: u64) -> BootRun {
    let mut rng = Rng(0xB007_5EED_1340_0001 ^ b.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut gm = Clock {
        ns: 7 * 86_400 * S,
        frac: 0.0,
    };
    let mut wall = Clock {
        ns: gm.ns + FLEET_D + b.boot_off_ns,
        frac: 0.0,
    };
    let mut core = PhaseLockCore::new();
    let mut word = 0.0f64;
    let mut prev_gm = gm.ns;
    let mut grace_until = 0u64;
    let mut join_at: Option<u64> = None;
    let mut out = BootRun {
        steps: Vec::new(),
        anchor_events_after_first: 0,
        drift_at_lock_ns: 0.0,
        line_err_ns: Vec::new(),
        wall_minus_gm: Vec::new(),
        worst_word_err_ppm: 0.0,
        freq_steps: 0,
    };
    for w in 0..windows {
        gm.advance(TRUE_DT_NS, 0.0);
        wall.advance(TRUE_DT_NS, b.osc_ppm + word);
        out.wall_minus_gm.push(wall.ns - gm.ns);
        let mut samples: Vec<i64> = (0..SAMPLES_PER_WINDOW)
            .map(|_| wall.ns - gm.ns + (b.delay_ns + rng.gauss() * PTP_NOISE_NS).round() as i64)
            .collect();
        samples.sort();
        let median = samples[SAMPLES_PER_WINDOW / 2];
        let dt = (gm.ns - prev_gm) as f64 / 1e9;
        prev_gm = gm.ns;
        let lock_at = lock_window(b.osc_ppm);
        let locked = w >= lock_at;
        // The rate servo stand-in (it acquires through the grace as well: a rate, not a phase).
        let servo = servo_word(b.osc_ppm, w);
        if w == lock_at {
            out.drift_at_lock_ns =
                (wall.ns - gm.ns) as f64 + b.delay_ns - core.anchor_ns().expect("anchored") as f64;
        }
        if join_at == Some(w) {
            // The follower's join at its next poll: `FLEET_D − D`, the drift since the anchor stays
            // as phase error (no grace overlap: nothing stepped before).
            let delta = FLEET_D - core.anchor_ns().expect("anchored");
            wall.ns += delta;
            core.note_step(delta);
            core.set_anchor(FLEET_D);
            out.steps.push((w, delta));
            grace_until = w + 1 + GRACE_WINDOWS;
            join_at = None;
            continue;
        }
        if w < grace_until {
            // The controller's 2 s post-step grace: the window is dropped, the word held.
            if !locked {
                word = servo;
            }
            continue;
        }
        let o = core.on_window(median, locked, servo, dt);
        match o.event {
            AnchorEvent::Anchored { anchor_ns } => {
                assert_eq!(
                    w + 1,
                    ANCHOR_CONFIRM_WINDOWS as u64,
                    "{}: anchored on the confirming window",
                    b.label
                );
                if b.join_lag_windows == 0 {
                    // The master restores the saved fleet D and steps its own wall onto it
                    // (`realign_master_to_fleet`) in the anchoring iteration.
                    let delta = FLEET_D - anchor_ns;
                    wall.ns += delta;
                    core.note_step(delta);
                    core.set_anchor(FLEET_D);
                    out.steps.push((w, delta));
                    grace_until = w + 1 + GRACE_WINDOWS;
                } else {
                    join_at = Some(w + b.join_lag_windows);
                }
            }
            AnchorEvent::None => {}
            _ => out.anchor_events_after_first += 1,
        }
        word = o.freq_ppm.unwrap_or(servo);
        if locked {
            let truth = -b.osc_ppm;
            out.worst_word_err_ppm = out.worst_word_err_ppm.max((word - truth).abs());
            out.line_err_ns
                .push((wall.ns - gm.ns) as f64 + b.delay_ns - FLEET_D as f64);
        }
    }
    out.freq_steps = core.freq_steps();
    out
}

#[test]
fn the_fleet_date_lands_at_the_first_window_and_the_lock_takes_no_second_step_134() {
    let lock_s = |w: u64| w as f64 * WINDOW_S;
    let master = BootBox {
        label: "master (strih-pp: RTC −3.75 s, −21 ppm)",
        osc_ppm: -21.0,
        delay_ns: 25_000.0,
        boot_off_ns: -3_750 * MS,
        join_lag_windows: 0,
        seed: 1,
    };
    let follower = BootBox {
        label: "follower (+40 ms, +70 ppm)",
        osc_ppm: 70.0,
        delay_ns: 58_000.0,
        boot_off_ns: 40 * MS,
        join_lag_windows: 2,
        seed: 2,
    };
    let windows = 15 * 60 * 2;
    let mut runs = Vec::new();
    for b in [&master, &follower] {
        assert!(lock_s(lock_window(b.osc_ppm)) >= LOCK_AFTER_MIN_S);
        let r = run_boot(b, windows);
        assert_eq!(r.steps.len(), 1, "{}: one step: {:?}", b.label, r.steps);
        let anchored_at = ANCHOR_CONFIRM_WINDOWS as u64 - 1;
        assert_eq!(
            r.steps[0].0,
            anchored_at + b.join_lag_windows,
            "{}: right after the early anchor",
            b.label
        );
        assert_eq!(
            r.anchor_events_after_first, 0,
            "{}: no re-anchor after the first window (no second step)",
            b.label
        );
        assert_eq!(
            r.freq_steps, 0,
            "{}: the pull is not a frequency step",
            b.label
        );
        // From 400 s after the lock: on the fleet line. (The rate servo hands over up to 3 ppm
        // short, and the PI learns that through its integrator, ~100 s critically damped,
        // exactly as at today's engagement.)
        let from = 800usize;
        let worst = r.line_err_ns[from..]
            .iter()
            .fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(
            worst < 50_000.0,
            "{}: |line error| up to {worst} ns from 400 s after the lock",
            b.label
        );
        let bound = ERROR_CLAMP_US / FSTEP_PULL_TAU_S + 10.0;
        assert!(
            r.worst_word_err_ppm < bound,
            "{}: the word left the tick by {} ppm (bound {bound})",
            b.label,
            r.worst_word_err_ppm
        );
        runs.push(r);
    }
    // The +70 ppm follower needed the hold: under the pre-1.19 rule its engagement would have
    // re-anchored and re-joined with a second step.
    assert!(
        runs[1].drift_at_lock_ns.abs() > REANCHOR_ON_ENGAGE_NS as f64,
        "the follower's drift at the lock: {} ns",
        runs[1].drift_at_lock_ns
    );
    // Both walls on one line (their path delays differ by 33 µs) from 400 s after the later lock.
    let from = (lock_window(master.osc_ppm).max(lock_window(follower.osc_ppm)) + 800) as usize;
    let worst = (from..windows as usize)
        .map(|w| (runs[0].wall_minus_gm[w] - runs[1].wall_minus_gm[w]).abs())
        .max()
        .expect("windows");
    assert!(worst < 100 * US, "walls {worst} ns apart");
}
