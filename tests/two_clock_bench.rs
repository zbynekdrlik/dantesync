//! dantesync#117 + #88 — the TWO-CLOCK BENCH.
//!
//! N simulated boxes on one Dante network, driven by the SAME code the controller runs
//! (`dantesync::ptp_phase_lock::PhaseLockCore` for every box's frequency word and anchor,
//! `dantesync::date_offset::{DateAuthority, DateFollower}` for the date), against two clocks that
//! disagree the way the real rig does:
//!
//! - a PTP source: the Dante grandmaster (device uptime, its own oscillator), which changes to a
//!   DIFFERENT grandmaster (another uptime, another oscillator) halfway through;
//! - an NTP (UTC) source that drifts against the grandmaster (+8 ppm measured live, the
//!   "0.7 s/day" of #117) — and, in a second run, −15 ppm.
//!
//! Every box has its own oscillator error (−40..+55 ppm) with slow thermal wander, its own
//! PTP path delay, software-timestamp noise, a ms-level boot NTP error, and lossy authority polls.
//! Box 0 is the NTP master (the date-offset authority); the others follow it over the (simulated)
//! 31900 poll, which is what `time_server::UdpAuthorityPoller` does on the wire.
//!
//! Proven over 24 simulated hours, for both UTC scenarios:
//!
//! 1. RATE = PTP: every box's effective rate equals the CURRENT grandmaster's rate (to 0.01 ppm per
//!    hour), and the frequency command sequence of every box is BIT-IDENTICAL across the two UTC
//!    scenarios — NTP contributes exactly nothing to the rate (the #117 decoupling statement).
//! 2. PHASE: after the join, all walls agree within 100 µs at every instant, including through
//!    the date steps and through the grandmaster change.
//! 3. DATE: every step after the join is a COORDINATED one, applied by every box in the same
//!    window; zero late or local steps; the master's wall stays within the step bound of UTC.
//! 4. GM CHANGE: no box steps its wall when the grandmaster changes.

use dantesync::date_offset::{
    same_time_base, DateAnnounce, DateAuthority, DateFollower, FollowAction, StepKind,
    DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS,
};
use dantesync::ptp_phase_lock::{AnchorEvent, PhaseLockCore};

const NS: i64 = 1;
const US: i64 = 1_000 * NS;
const MS: i64 = 1_000 * US;
const S: i64 = 1_000 * MS;

/// PTP sample window: 4 Sync messages at 8/s (the controller's `sample_window_size` = 4).
const WINDOW_S: f64 = 0.5;
const SAMPLES_PER_WINDOW: usize = 4;
/// Software-timestamp noise per PTP sample (σ).
const PTP_NOISE_NS: f64 = 20_000.0;
/// Master's NTP-to-UTC measurement noise (σ) — a WAN upstream.
const NTP_NOISE_NS: f64 = 400_000.0;
const NTP_INTERVAL_WINDOWS: u64 = 20; // 10 s, the master's cadence
const POLL_INTERVAL_WINDOWS: u64 = 2; // 1 s
const POLL_LOSS: f64 = 0.10;
const HOURS: u64 = 24;
const GM_CHANGE_AT_WINDOW: u64 = 12 * 3600 * 2 + 1_234;
const GM_REBOOT_AT_WINDOW: u64 = 18 * 3600 * 2 + 777;
/// The master notices a grandmaster event this many windows after the first follower does.
const MASTER_EVENT_LAG_WINDOWS: u64 = 5;
/// The controller's 2 s post-step grace, in 0.5 s windows.
const GRACE_WINDOWS: u64 = 4;

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// A clock whose reading is an integer ns + a fractional carry (a wall of 1.79e18 ns does not fit
/// an f64 at ns resolution).
#[derive(Clone, Copy)]
struct Clock {
    ns: i64,
    frac: f64,
}
impl Clock {
    fn advance(&mut self, true_dt_ns: f64, rate_ppm: f64) {
        let d = true_dt_ns * (1.0 + rate_ppm * 1e-6) + self.frac;
        let whole = d.floor();
        self.frac = d - whole;
        self.ns += whole as i64;
    }
}

struct Box_ {
    osc_ppm: f64,
    wander_ppm: f64,
    wander_period_s: f64,
    delay_ns: f64,
    /// The oscillator-driven part of the wall. Steps are kept apart in `stepped`, so a step
    /// never perturbs the ns-carry of the continuous clock: two runs that step at different
    /// times still integrate bit-identical oscillator paths.
    wall: Clock,
    stepped: i64,
    word_ppm: f64,
    core: PhaseLockCore,
    follower: DateFollower,
    /// The grandmaster the phase-lock anchor belongs to (the controller keeps `current_gm_uuid`).
    core_gm: u8,
    /// How many windows after the grandmaster change / reboot this box hears it.
    lag: (u64, u64),
    /// PTP windows before this one are dropped (the post-step grace), `grace` runs only.
    grace_until: u64,
    rng: Rng,
    // metrics
    words: Vec<f64>,
    /// Every applied step: (announce seq, size ns, kind, TRUE time it landed, ns since start).
    steps: Vec<Step>,
    rebases: u32,
}

type Step = (u32, i64, StepKind, f64);

struct RunResult {
    words: Vec<Vec<f64>>,
    steps: Vec<Vec<Step>>,
    max_disagreement_ns: i64,
    /// Window boundaries at which a coordinated step was in flight (some boxes past its instant,
    /// others a few µs short of it): the disagreement there is the step itself, and its
    /// simultaneity is judged by the landing-time spread instead.
    in_flight_samples: u32,
    max_utc_error_ns: i64,
    rate_errors_ppm: Vec<f64>,
    rebases: Vec<u32>,
    late_steps: Vec<u32>,
    /// Follower polls refused as another time base (another GM, or a GM that rebooted).
    refused_replies: u32,
}

/// What each box hears: the grandmaster's UUID and its time base. The grandmaster CHANGES (to
/// another device: another UUID, uptime and oscillator) at `GM_CHANGE_AT_WINDOW`, and that new
/// grandmaster REBOOTS under the same UUID (its uptime restarts) at `GM_REBOOT_AT_WINDOW`. Each box
/// notices each event a few windows apart. At the change the MASTER is last, so followers
/// re-anchor while it still publishes a `D` in the old base (refused by the anchor grandmaster in
/// the extension). At the reboot the master is FIRST, so it publishes a `D` in the new base while
/// some followers are still in the old one under the SAME UUID: only the time-base check
/// (`same_time_base`) stops those from taking a multi-day "late" step.
fn gm_view<'a>(
    w: u64,
    lags: (u64, u64),
    a: &'a Clock,
    b_pre: &'a Clock,
    b_post: &'a Clock,
) -> (u8, &'a Clock) {
    let (change_lag, reboot_lag) = lags;
    if w < GM_CHANGE_AT_WINDOW + change_lag {
        (1, a)
    } else if w < GM_REBOOT_AT_WINDOW + reboot_lag {
        (2, b_pre)
    } else {
        (2, b_post)
    }
}

fn run(utc_vs_gm_ppm: f64, grace: bool) -> RunResult {
    let true_dt_ns = WINDOW_S * 1e9;
    let base_utc: i64 = 1_790_000_000 * S;

    // Grandmaster A: 3 days of uptime. B: 11 days of uptime, a different oscillator (+3 ppm);
    // after its reboot the same B restarts at 42 s of uptime.
    let mut gm_a = Clock {
        ns: 3 * 86_400 * S,
        frac: 0.0,
    };
    let mut gm_b_pre = Clock {
        ns: 11 * 86_400 * S,
        frac: 0.0,
    };
    let mut gm_b_post = Clock {
        ns: 42 * S - (GM_REBOOT_AT_WINDOW as i64) * (WINDOW_S * 1e9) as i64,
        frac: 0.0,
    };
    let (gm_a_ppm, gm_b_ppm) = (0.0, 3.0);
    let mut utc = Clock {
        ns: base_utc,
        frac: 0.0,
    };

    let oscs = [23.0, -41.0, 54.0, 9.5, -17.0, 31.0];
    let delays = [25_000.0, 32_000.0, 38_000.0, 44_000.0, 51_000.0, 58_000.0];
    let boot_err = [
        0,
        2_100 * US,
        -3_400 * US,
        900 * US,
        -1_700 * US,
        4_200 * US,
    ];
    // Event lags (windows): the master (box 0) notices the grandmaster CHANGE last and the
    // grandmaster REBOOT first (see `gm_view`).
    let change_lags = [MASTER_EVENT_LAG_WINDOWS, 0, 3, 1, 2, 4];
    let reboot_lags = [0, 6, 3, 5, 2, 4];
    let mut boxes: Vec<Box_> = (0..oscs.len())
        .map(|i| Box_ {
            osc_ppm: oscs[i],
            wander_ppm: 0.2 + 0.05 * i as f64,
            wander_period_s: 3_600.0 + 1_300.0 * i as f64,
            delay_ns: delays[i],
            wall: Clock {
                ns: base_utc + boot_err[i],
                frac: 0.0,
            },
            stepped: 0,
            // The rate servo hands over a word that is 0.4 ppm off the truth.
            word_ppm: -oscs[i] + 0.4,
            core: PhaseLockCore::new(),
            follower: DateFollower::new(),
            core_gm: 1,
            lag: (change_lags[i], reboot_lags[i]),
            grace_until: 0,
            rng: Rng(0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1) * 0x1000_0000_01B3),
            words: Vec::new(),
            steps: Vec::new(),
            rebases: 0,
        })
        .collect();

    let mut authority: Option<DateAuthority> = None;
    let mut ntp_rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut net_rng = Rng(0xABCD_EF01_2345_6789);

    let windows = HOURS * 3600 * 2;
    let mut max_dis = 0i64;
    let mut in_flight = 0u32;
    let mut max_utc = 0i64;
    let mut refused = 0u32;
    // Windows (30 s) after which every box has joined — the join happens within seconds.
    let all_joined_after = 60;
    // Per-hour effective-rate audit of every box vs the grandmaster it currently hears.
    let mut hour_start: Vec<(i64, i64)> = vec![(0, 0); boxes.len()]; // (continuous wall, gm)
    let mut rate_errors = Vec::new();
    let near_event = |w: u64| {
        [GM_CHANGE_AT_WINDOW, GM_REBOOT_AT_WINDOW]
            .iter()
            .any(|&e| (w.saturating_sub(7_200)..w).contains(&e) || (e..e + 8).contains(&w))
    };

    for w in 0..windows {
        let t_now_s = w as f64 * WINDOW_S;
        // 1. true time advances; every clock ticks at its own rate. A box whose wall crosses the
        //    instant of its scheduled coordinated step inside this window applies it AT the
        //    crossing (the controller polls `due` every loop iteration, 1 ms / 50 µs), so the
        //    landing instant is resolved below the window.
        let t0_ns = w as f64 * true_dt_ns;
        utc.advance(true_dt_ns, utc_vs_gm_ppm);
        gm_a.advance(true_dt_ns, gm_a_ppm);
        gm_b_pre.advance(true_dt_ns, gm_b_ppm);
        gm_b_post.advance(true_dt_ns, gm_b_ppm);
        for b in boxes.iter_mut() {
            let wander =
                b.wander_ppm * (2.0 * std::f64::consts::PI * t_now_s / b.wander_period_s).sin();
            let rate = b.osc_ppm + wander + b.word_ppm;
            let scale = 1.0 + rate * 1e-6;
            let start = b.wall_ns();
            let crossing = b.follower.pending().and_then(|p| {
                let to_cross = (p.effective_wall_ns - start) as f64 / scale;
                (p.effective_wall_ns > start && to_cross <= true_dt_ns).then_some((p, to_cross))
            });
            b.wall.advance(true_dt_ns, rate);
            if let Some((p, to_cross)) = crossing {
                let due = b.follower.due(p.effective_wall_ns).expect("at the instant");
                b.apply_step(
                    due.seq,
                    due.delta_ns,
                    StepKind::Coordinated,
                    t0_ns + to_cross,
                    w,
                    grace,
                );
            }
        }

        // 2. a scheduled step whose instant is already behind the wall (scheduled at or after
        //    its own instant) — applied at the boundary. Never expected; the checks reject it.
        for b in boxes.iter_mut() {
            if let Some(due) = b.follower.due(b.wall_ns()) {
                b.apply_step(
                    due.seq,
                    due.delta_ns,
                    StepKind::Late,
                    t0_ns + true_dt_ns,
                    w,
                    grace,
                );
            }
        }

        // 3. one PTP window per box: median of noisy (t2 − t1) against the grandmaster it hears.
        //    With `grace` the controller's 2 s post-step grace is modelled: those windows are
        //    dropped and the word is held.
        let mut master_rebase: Option<(i64, i64)> = None;
        for (i, b) in boxes.iter_mut().enumerate() {
            let (gm_id, gm) = gm_view(w, b.lag, &gm_a, &gm_b_pre, &gm_b_post);
            let mut samples: Vec<i64> = (0..SAMPLES_PER_WINDOW)
                .map(|_| {
                    let noise = b.rng.gauss() * PTP_NOISE_NS;
                    // wall_rx − gm_tx = (wall − gm) + delay, plus the timestamp noise.
                    b.wall_ns() - gm.ns + (b.delay_ns + noise).round() as i64
                })
                .collect();
            samples.sort();
            let median = samples[SAMPLES_PER_WINDOW / 2];
            if gm_id != b.core_gm {
                // The controller's GRANDMASTER UUID CHANGED path. (A reboot under the same UUID
                // is caught by the core's own time-base discontinuity check.)
                b.core.request_rebase();
                b.core_gm = gm_id;
            }
            if w < b.grace_until {
                b.words.push(b.word_ppm);
                continue;
            }
            let out = b.core.on_window(median, true, b.word_ppm, WINDOW_S);
            if let AnchorEvent::Rebased { old_ns, new_ns } = out.event {
                b.rebases += 1;
                if i == 0 {
                    master_rebase = Some((old_ns, new_ns));
                }
            }
            b.word_ppm = out.freq_ppm.expect("locked from the start: always engaged");
            b.words.push(b.word_ppm);
        }

        // 4. the master: authority lifecycle + UTC every 10 s (the controller's date_sync glue).
        {
            let m = &mut boxes[0];
            let anchor = m.core.anchor_ns().unwrap();
            let now_ptp = m.wall_ns() - anchor;
            if authority.is_none() {
                let a =
                    DateAuthority::new(anchor, now_ptp, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS);
                let act = m.follower.on_announce(a.announce(), anchor, m.wall_ns());
                assert_eq!(
                    act,
                    FollowAction::None,
                    "the authority is aligned with itself"
                );
                authority = Some(a);
            }
            let a = authority.as_mut().unwrap();
            if let Some((old_ns, new_ns)) = master_rebase {
                // The master's re-anchor IS a rebase of the fleet offset (no step).
                a.rebase(new_ns, m.wall_ns() - old_ns);
            }
            if w % NTP_INTERVAL_WINDOWS == 0 && w > 0 {
                let err = utc.ns - m.wall_ns() + (ntp_rng.gauss() * NTP_NOISE_NS).round() as i64;
                if let Some(ann) = a.on_utc_error(err, now_ptp) {
                    let act = m.follower.on_announce(ann, anchor, m.wall_ns());
                    assert!(
                        matches!(act, FollowAction::Scheduled { .. }),
                        "master schedules its own step: {act:?}"
                    );
                }
            }
            if w > 20 {
                max_utc = max_utc.max((utc.ns - m.wall_ns()).abs());
            }
        }

        // 5. followers poll the master's 31900 every second (10 % of polls lost). The reply
        //    carries the master's wall, its published D and the D's grandmaster (the extension);
        //    a follower adopts it only in its own time base — the controller's exact checks.
        if w % POLL_INTERVAL_WINDOWS == 0 {
            let master_gm = boxes[0].core_gm;
            let master_wall = boxes[0].wall_ns();
            let ann: DateAnnounce = authority.as_ref().unwrap().announce();
            for b in boxes.iter_mut().skip(1) {
                if net_rng.uniform() < POLL_LOSS {
                    continue;
                }
                let Some(anchor) = b.core.anchor_ns() else {
                    continue;
                };
                if b.core.rebase_pending()
                    || b.core_gm != master_gm
                    || !same_time_base(master_wall, ann.date_offset_ns, b.wall_ns(), anchor)
                {
                    refused += 1;
                    continue;
                }
                match b.follower.on_announce(ann, anchor, b.wall_ns()) {
                    FollowAction::None | FollowAction::Scheduled { .. } => {}
                    FollowAction::Absorb { new_anchor_ns } => b.core.set_anchor(new_anchor_ns),
                    FollowAction::Step { delta_ns, kind } => {
                        b.apply_step(ann.seq, delta_ns, kind, t0_ns + true_dt_ns, w, grace)
                    }
                }
            }
        }

        // 6. metrics.
        if w > all_joined_after {
            // Raw wall disagreement at the same true instant, INCLUDING each box's own PTP path
            // delay (a box holds `t2 − t1 = D`, so its wall sits `delay` behind the GM line) —
            // exactly what a cross-box genlock grid sees.
            let applied = |b: &Box_| {
                b.steps
                    .iter()
                    .filter(|s| s.2 == StepKind::Coordinated)
                    .count()
            };
            if boxes.iter().all(|b| applied(b) == applied(&boxes[0])) {
                let hi = boxes.iter().map(|b| b.wall_ns()).max().unwrap();
                let lo = boxes.iter().map(|b| b.wall_ns()).min().unwrap();
                max_dis = max_dis.max(hi - lo);
            } else {
                in_flight += 1;
            }
        }
        if w % 7_200 == 0 {
            // Hourly effective rate vs the grandmaster each box hears (hours around a
            // grandmaster event mix two rates / two bases by construction and are skipped).
            for (i, b) in boxes.iter().enumerate() {
                let (_, gm) = gm_view(w, b.lag, &gm_a, &gm_b_pre, &gm_b_post);
                let (w0, g0) = hour_start[i];
                if w > 7_200 && !near_event(w) {
                    // The continuous clock (the wall without its steps) against the GM's time.
                    let d_wall = (b.wall.ns - w0) as f64;
                    let d_gm = (gm.ns - g0) as f64;
                    rate_errors.push((d_wall / d_gm - 1.0) * 1e6);
                }
                hour_start[i] = (b.wall.ns, gm.ns);
            }
        }
    }

    RunResult {
        words: boxes.iter().map(|b| b.words.clone()).collect(),
        steps: boxes.iter().map(|b| b.steps.clone()).collect(),
        max_disagreement_ns: max_dis,
        in_flight_samples: in_flight,
        max_utc_error_ns: max_utc,
        rate_errors_ppm: rate_errors,
        rebases: boxes.iter().map(|b| b.rebases).collect(),
        late_steps: boxes.iter().map(|b| b.follower.late_steps()).collect(),
        refused_replies: refused,
    }
}

impl Box_ {
    fn wall_ns(&self) -> i64 {
        self.wall.ns + self.stepped
    }

    /// Step the wall and move D with it (the controller's `apply_date_step`), recording it; with
    /// `grace`, the next 2 s of PTP windows are dropped as the controller does after any step.
    fn apply_step(
        &mut self,
        seq: u32,
        delta_ns: i64,
        kind: StepKind,
        t_ns: f64,
        w: u64,
        grace: bool,
    ) {
        self.stepped += delta_ns;
        self.core.note_step(delta_ns);
        self.steps.push((seq, delta_ns, kind, t_ns));
        if grace {
            self.grace_until = w + 1 + GRACE_WINDOWS;
        }
    }
}

fn check(label: &str, r: &RunResult) {
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

    // DATE: the master's own steps are all coordinated; each follower joined once, then took
    // exactly the master's steps (same seq, same size), with zero late/uncoordinated steps.
    let master_steps = &r.steps[0];
    assert!(
        !master_steps.is_empty(),
        "[{label}] the master never stepped the date"
    );
    assert!(master_steps.iter().all(|s| s.2 == StepKind::Coordinated));
    for (i, steps) in r.steps.iter().enumerate().skip(1) {
        let (joins, rest): (Vec<&Step>, Vec<&Step>) =
            steps.iter().partition(|s| s.2 == StepKind::Join);
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
        let seq_size = |v: &[&Step]| v.iter().map(|s| (s.0, s.1)).collect::<Vec<_>>();
        let master: Vec<&Step> = master_steps.iter().collect();
        assert_eq!(
            seq_size(&rest),
            seq_size(&master),
            "[{label}] box {i} did not take exactly the master's steps"
        );
    }
    // SIMULTANEITY: every coordinated step landed on every box within 100 µs of true time.
    let mut worst_spread = 0.0f64;
    for (k, ms) in master_steps.iter().enumerate() {
        let landings: Vec<f64> = r
            .steps
            .iter()
            .map(|steps| {
                steps
                    .iter()
                    .filter(|s| s.2 == StepKind::Coordinated)
                    .nth(k)
                    .expect("same number of coordinated steps")
                    .3
            })
            .collect();
        let spread = landings.iter().cloned().fold(f64::MIN, f64::max)
            - landings.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            spread < 100_000.0,
            "[{label}] step seq {} landed across {spread} ns",
            ms.0
        );
        worst_spread = worst_spread.max(spread);
    }
    println!(
        "[{label}] {} coordinated steps, worst landing spread {:.1} µs, {} in-flight samples",
        master_steps.len(),
        worst_spread / 1_000.0,
        r.in_flight_samples
    );
    assert!(
        r.late_steps.iter().all(|&l| l == 0),
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
    assert_eq!(n, 6);
}

#[test]
fn two_clock_bench_rate_is_ptp_phase_agrees_and_every_date_step_is_coordinated_117_88() {
    let plus = run(8.0, false);
    check("UTC +8 ppm vs GM", &plus);
    let minus = run(-15.0, false);
    check("UTC -15 ppm vs GM", &minus);

    // THE DECOUPLING STATEMENT, for the frequency LAW: the two runs share every PTP input and
    // differ ONLY in UTC (so in the number, size and timing of the date steps). The frequency
    // command of every box is bit-identical between them: NTP contributes nothing to the rate.
    for i in 0..plus.words.len() {
        assert_eq!(
            plus.words[i], minus.words[i],
            "box {i}: the frequency word depends on UTC — NTP leaked into the rate path"
        );
    }
    // … while the date genuinely differed (the runs are not trivially identical).
    assert_ne!(plus.steps[0].len(), minus.steps[0].len());
    let dir = |r: &RunResult| r.steps[0].iter().map(|s| s.1.signum()).sum::<i64>();
    assert!(
        dir(&plus) > 0 && dir(&minus) < 0,
        "UTC ahead ⇒ forward steps, behind ⇒ backward"
    );
}

#[test]
fn with_the_controllers_post_step_grace_the_envelopes_still_hold_117_88() {
    // The controller drops 2 s of PTP windows after every step (the step transient), holding the
    // word. Those windows fall at UTC-dependent times, so the words are no longer bit-identical
    // across UTC scenarios — the hold carries no NTP value, it only delays the next update. What
    // must survive is every envelope: rate = the GM's, walls within 100 µs, only coordinated
    // steps, no step at a grandmaster change or reboot.
    check("UTC +8 ppm vs GM, with grace", &run(8.0, true));
    check("UTC -15 ppm vs GM, with grace", &run(-15.0, true));
}
