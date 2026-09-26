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
//! Two harder scenarios run on top: the master boots 3 s off UTC (its first announce is a 3 s
//! step, far larger than any time-base tolerance), and ONLY the master loses PTP for 10 minutes
//! (a NIC / pcap fault on one box) while the fleet keeps it.
//!
//! Proven over 24 simulated hours, for both UTC scenarios:
//!
//! 1. RATE = PTP: every box's effective rate equals the CURRENT grandmaster's rate (to 0.01 ppm per
//!    hour), and the frequency command sequence of every box is BIT-IDENTICAL across the two UTC
//!    scenarios — NTP contributes exactly nothing to the rate (the #117 decoupling statement).
//! 2. PHASE: after the join, all walls agree within 100 µs at every instant, including through
//!    the date steps and through the grandmaster change (within 300 µs while the fleet settles
//!    the one documented double fault: a grandmaster change during a master-only PTP outage).
//! 3. DATE: every step after the join is a COORDINATED one, applied by every box in the same
//!    window; zero late or local steps; the master's wall stays within the step bound of UTC.
//! 4. GM CHANGE: no box steps its wall when the grandmaster changes.
//!
//! dantesync#119: a BACKWARD correction (UTC −15 ppm vs the GM: the fleet runs ahead) is a
//! coordinated SLEW, never a step. The bench mirrors the controller's slew glue — the rate term in
//! the frequency word (applied at the start/end instants, so resolved inside the window like a
//! step's landing) and the de-slew of every PTP sample — and proves: no box ever steps backwards
//! or runs its wall backwards, the relative phase of the fleet (each wall plus its own PTP path
//! delay) stays within 50 µs through every slew, and the phase lock's law is untouched (the
//! words differ from a forward-only run by numerical noise only).
//!
//! dantesync#119 follow-up: the fleet date is corrected in MICRO-corrections — at most 500 µs, at
//! most one per 20 s, beyond a 2 ms dead band, decided on the master's loop tick. The bench drives
//! that tick every window and proves, over 24 h at +17.6 ppm (the rig's drift) and at −15 ppm:
//! no single correction above 500 µs, the fleet date within 3 ms of UTC, the relative phase within
//! 50 µs, no large step; and that ±5 ms asymmetric UTC jitter never makes the corrections
//! oscillate.

use dantesync::date_offset::{
    same_time_base, slew_cap_ns, DateAnnounce, DateAuthority, DateFollower, FollowAction, SlewSpec,
    StepKind, DEFAULT_SLEW_PPM, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS,
};
use dantesync::ptp_phase_lock::{AnchorEvent, PhaseLockCore};

const NS: i64 = 1;
const US: i64 = 1_000 * NS;
const MS: i64 = 1_000 * US;
const S: i64 = 1_000 * MS;

/// #119 follow-up: the largest micro-correction (the default `micro_step_us`).
const MICRO_STEP_NS: i64 = dantesync::date_offset::DEFAULT_MICRO_STEP_US as i64 * US;

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
/// The master's legacy NTP step threshold while it runs the local date path (PTP offline).
const MASTER_LOCAL_THRESHOLD_NS: i64 = 200_000;
/// Wall disagreement allowed while the fleet settles the double fault (a grandmaster change
/// during a master-only PTP outage): the master's untracked free-run error over its outage (about
/// 100 µs here) on top of the normal 100 µs envelope, with margin. Measured: 155 µs.
const SETTLING_BOUND_NS: i64 = 300 * US;

struct Scenario {
    label: &'static str,
    utc_vs_gm_ppm: f64,
    /// Model the controller's 2 s post-step grace (drop the PTP windows, hold the word).
    grace: bool,
    /// The master's wall error vs UTC at boot (the followers' are fixed, ms-level).
    master_boot_err_ns: i64,
    /// Windows `[from, to)` in which ONLY the master hears no PTP.
    master_ptp_offline: Vec<(u64, u64)>,
    /// The grandmaster changes DURING one of those outages (a double fault). The master then
    /// re-bases the fleet D with its own untracked free-run error (it had no PTP to measure it),
    /// which followers take as one small late step: allowed, bounded to < 1 ms.
    gm_change_in_master_outage: bool,
    /// #119: UTC jumps (window, ns) — an upstream stepping back makes the fleet run ahead at once,
    /// so backward (micro-)corrections follow. The master's UTC
    /// bound is not judged for `UTC_JUMP_SETTLE_WINDOWS` after each jump.
    utc_jumps: Vec<(u64, i64)>,
    /// #119 ROZHODNUTÉ: this scenario MEANS to cause a backward correction beyond the slew cap
    /// (a master booting seconds ahead); only then does `check()` accept a backward step.
    expects_too_large_step: bool,
    /// #119: the rate of a backward correction's slew (the config's `slew_ppm`).
    slew_ppm: u32,
    /// #119 follow-up: the master's and the fleet's UTC error are judged only after this many
    /// windows (the join and the first micro estimate; longer where a boot error is worked off
    /// in micro-corrections).
    settle_windows: u64,
    /// #119 follow-up: the master's UTC reading noise.
    ntp_noise: NtpNoise,
    /// #119 follow-up: mixed into every noise source's seed (0 = the historical noise), so a
    /// statistic can be asserted over several noise samples (the "seed every noise source" rule).
    seed: u64,
    /// #119 (1.11.1): the boxes running Windows — every step is realized by the production step
    /// law (`dantesync::clock::step`) against a model of the Windows clock, and every PTP sample is
    /// stamped by that stepped clock at its capture instant (`two_clock_bench/windows.rs`). The
    /// others step ideally, as before.
    windows_boxes: &'static [usize],
    /// How many 0.5 s windows the run lasts.
    run_windows: u64,
}

/// #119: after a UTC jump the fleet is off UTC by the jump until the corrections have paid it.
/// Since the micro-corrections (v1.11) a jump of up to 30 ms is worked off at the capacity (the
/// jumps scenario's 25 ppm slews are 30 s apart — two 5 s leads + a 20 s slew — so ~1 ms/min:
/// 30 ms in ~30 min): 50 minutes covers it.
const UTC_JUMP_SETTLE_WINDOWS: u64 = 6_000;

impl Scenario {
    fn plain(label: &'static str, utc_vs_gm_ppm: f64, grace: bool) -> Self {
        Scenario {
            label,
            utc_vs_gm_ppm,
            grace,
            master_boot_err_ns: 0,
            master_ptp_offline: Vec::new(),
            gm_change_in_master_outage: false,
            utc_jumps: Vec::new(),
            expects_too_large_step: false,
            slew_ppm: DEFAULT_SLEW_PPM,
            settle_windows: 240,
            ntp_noise: NtpNoise::Gauss,
            seed: 0,
            windows_boxes: &[],
            run_windows: HOURS * 3600 * 2,
        }
    }
    fn settling_after_a_utc_jump(&self, w: u64) -> bool {
        self.utc_jumps
            .iter()
            .any(|&(at, _)| (at..at + UTC_JUMP_SETTLE_WINDOWS).contains(&w))
    }
    fn master_offline_at(&self, w: u64) -> bool {
        self.master_ptp_offline
            .iter()
            .any(|&(from, to)| (from..to).contains(&w))
    }
}

// The controller glue this bench mirrors lives in `two_clock_bench/glue.rs` (one place to mirror a
// change of `src/controller/date_sync.rs`).
#[path = "two_clock_bench/glue.rs"]
mod glue;
use glue::*;
#[path = "two_clock_bench/world.rs"]
mod world;
use world::*;
// #119 (1.11.1): the Windows clock model and its scenario.
#[path = "two_clock_bench/windows.rs"]
mod windows;
use windows::*;
#[path = "two_clock_bench/box_ops.rs"]
mod box_ops;

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
    /// #119: the wall's slew part — the integral of the slew's rate term, kept apart from the
    /// oscillator-driven clock like the steps (so the hourly rate audit still reads the PI law).
    slewed_ns: i64,
    slew_frac: f64,
    word_ppm: f64,
    core: PhaseLockCore,
    follower: DateFollower,
    /// The grandmaster the phase-lock anchor belongs to (the controller keeps `current_gm_uuid`).
    core_gm: u8,
    /// How many windows after the grandmaster change / reboot this box hears it.
    lag: (u64, u64),
    /// PTP windows before this one are dropped (the post-step grace), `grace` runs only.
    grace_until: u64,
    /// A phase-lock window ran since the last step / PTP outage (the controller's `fresh_window`).
    fresh: bool,
    rng: Rng,
    // metrics
    words: Vec<f64>,
    /// Every applied step: (announce seq, size ns, kind, TRUE time it landed, ns since start).
    steps: Vec<Step>,
    rebases: u32,
    /// #119: the wall at the previous window boundary (it must never go back after the join).
    last_wall: i64,
    /// #119: the master's catch-ups with a fleet slew its own scheduler missed.
    catch_ups: u32,
    /// #119 (1.11.1): the Windows clock model, on a Windows box.
    win: Option<WinClock>,
    /// The box's effective rate (ppm) and thermal wander in the current window.
    rate_ppm: f64,
    wander_now_ppm: f64,
    /// The last step: (TRUE time it landed, the wall's realized move).
    last_landing: Option<(f64, i64)>,
    /// #119 (1.11.1): the rate audit after the settle — the integrator (the learned rate) and the
    /// 20 s mean of the word against the truth (the grandmaster's rate minus the oscillator's).
    rate_audit: RateAudit,
}

type Step = (u32, i64, StepKind, f64);

struct RunResult {
    words: Vec<Vec<f64>>,
    steps: Vec<Vec<Step>>,
    max_disagreement_ns: i64,
    /// The worst wall disagreement while the fleet settles the double fault (only the scenario
    /// with a grandmaster change during a master outage has such a window; 0 otherwise).
    max_settling_disagreement_ns: i64,
    /// Window boundaries at which a coordinated step was in flight (some boxes past its instant,
    /// others a few µs short of it): the disagreement there is the step itself, and its
    /// simultaneity is judged by the landing-time spread instead.
    in_flight_samples: u32,
    max_utc_error_ns: i64,
    max_fleet_utc_error_ns: i64,
    rate_errors_ppm: Vec<f64>,
    rebases: Vec<u32>,
    late_steps: Vec<u32>,
    /// Every coordinated step the authority announced: (seq, size).
    announced: Vec<(u32, i64)>,
    /// #119: every coordinated slew it announced: (seq, amount).
    announced_slews: Vec<(u32, i64)>,
    /// #119 follow-up: (announced seq, the rebase seq it was re-announced under).
    renamed_steps: Vec<(u32, u32)>,
    /// Follower polls refused as another time base (another GM, or a GM that rebooted).
    refused_replies: u32,
    /// #119: the fleet's worst RELATIVE phase — the spread of (wall + own PTP path delay), i.e.
    /// the wall disagreement without the static receive-latency asymmetry — overall after the
    /// join, and while a slew runs.
    max_relative_phase_ns: i64,
    max_relative_phase_in_slew_ns: i64,
    /// #119: windows measured while a slew ran (so the in-slew bound is not vacuous).
    slew_windows: u32,
    /// #119: times any box's wall read less than at the previous window boundary.
    wall_went_back: u32,
    /// #119 follow-up: every announced correction in order: (window, size).
    corrections: Vec<(u64, i64)>,
    gm_events_in_slew: u32,
    /// The master's catch-ups with a fleet slew its own scheduler missed.
    master_catch_ups: u32,
    /// #119 (1.11.1): per box, the rate audit after the settle.
    rate_audits: Vec<RateAudit>,
    /// #119 (1.11.1): per box, the largest |requested − realized| of a step (0 on an ideal box).
    max_step_residual_ns: Vec<i64>,
}

/// Everything one bench run evolves: the true clocks, the boxes, the master's authority and its
/// published snapshot, and the metrics. `run` steps it one 0.5 s window at a time through the
/// phases below, in the order the real system runs them.
struct Bench<'s> {
    sc: &'s Scenario,
    gm_a: Clock,
    gm_b_pre: Clock,
    gm_b_post: Clock,
    utc: Clock,
    boxes: Vec<Box_>,
    authority: Option<DateAuthority>,
    snapshot: Option<Published>,
    master_local_candidate: Option<i64>,
    /// Every coordinated step the authority announced: (seq, size).
    announced: Vec<(u32, i64)>,
    announced_slews: Vec<(u32, i64)>,
    /// #119 follow-up: (announced seq, the rebase seq it was re-announced under).
    renamed_steps: Vec<(u32, u32)>,
    /// #119 follow-up: every correction the authority announced, in order: (window, size) — a
    /// step's size or a slew's amount.
    corrections: Vec<(u64, i64)>,
    /// #119: grandmaster events (change, reboot) that happened while the fleet slewed.
    gm_events_in_slew: u32,
    ntp_rng: Rng,
    net_rng: Rng,
    // metrics
    max_dis: i64,
    /// The worst disagreement inside the double fault's settling window (excluded from
    /// `max_dis`, bounded on its own).
    max_settling_dis: i64,
    in_flight: u32,
    max_utc: i64,
    /// The FLEET LINE's UTC error, read on a follower (box 1): what every genlocked box shows.
    max_fleet_utc: i64,
    refused: u32,
    max_rel: i64,
    max_rel_slew: i64,
    slew_windows: u32,
    wall_back: u32,
    /// Per-hour effective-rate audit of every box vs the grandmaster it currently hears:
    /// (continuous wall, gm) at the start of the hour.
    hour_start: Vec<(i64, i64)>,
    rate_errors: Vec<f64>,
}

/// One PTP window's result for the master's glue: its re-anchor on a new time base, if any, and
/// whether its phase-lock window ran at all (it has no PTP during its own outage).
struct MasterWindow {
    rebase: Option<(i64, i64)>,
    ran: bool,
}

const TRUE_DT_NS: f64 = WINDOW_S * 1e9;
/// The grandmasters' oscillators (ppm vs true time): A, then B after the change.
const GM_A_PPM: f64 = 0.0;
const GM_B_PPM: f64 = 3.0;
/// Windows (30 s) after which every box has joined — the join happens within seconds.
const ALL_JOINED_AFTER: u64 = 60;

impl<'s> Bench<'s> {
    fn new(sc: &'s Scenario) -> Self {
        let base_utc: i64 = 1_790_000_000 * S;
        let oscs = [23.0, -41.0, 54.0, 9.5, -17.0, 31.0];
        let delays = [25_000.0, 32_000.0, 38_000.0, 44_000.0, 51_000.0, 58_000.0];
        let boot_err = [
            sc.master_boot_err_ns,
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
        let boxes: Vec<Box_> = (0..oscs.len())
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
                slewed_ns: 0,
                slew_frac: 0.0,
                // The rate servo hands over a word that is 0.4 ppm off the truth.
                word_ppm: -oscs[i] + 0.4,
                core: PhaseLockCore::new(),
                follower: DateFollower::new(),
                core_gm: 1,
                lag: (change_lags[i], reboot_lags[i]),
                grace_until: 0,
                fresh: false,
                rng: Rng(
                    (0x9E37_79B9_7F4A_7C15 ^ ((i as u64 + 1) * 0x1000_0000_01B3))
                        ^ sc.seed.wrapping_mul(0x2545_F491_4F6C_DD1D),
                ),
                words: Vec::new(),
                steps: Vec::new(),
                rebases: 0,
                last_wall: 0,
                catch_ups: 0,
                win: sc
                    .windows_boxes
                    .contains(&i)
                    .then(|| WinClock::new(i, sc.seed)),
                rate_ppm: 0.0,
                wander_now_ppm: 0.0,
                last_landing: None,
                rate_audit: RateAudit::default(),
            })
            .collect();
        let n = boxes.len();
        Bench {
            sc,
            // Grandmaster A: 3 days of uptime. B: 11 days of uptime, a different oscillator
            // (+3 ppm, see `advance_clocks`); after its reboot the same B restarts at 42 s of
            // uptime.
            gm_a: Clock {
                ns: 3 * 86_400 * S,
                frac: 0.0,
            },
            gm_b_pre: Clock {
                ns: 11 * 86_400 * S,
                frac: 0.0,
            },
            gm_b_post: Clock {
                ns: 42 * S - (GM_REBOOT_AT_WINDOW as i64) * TRUE_DT_NS as i64,
                frac: 0.0,
            },
            utc: Clock {
                ns: base_utc,
                frac: 0.0,
            },
            boxes,
            authority: None,
            snapshot: None,
            master_local_candidate: None,
            announced: Vec::new(),
            announced_slews: Vec::new(),
            renamed_steps: Vec::new(),
            corrections: Vec::new(),
            gm_events_in_slew: 0,
            ntp_rng: Rng(0xD1B5_4A32_D192_ED03 ^ sc.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            net_rng: Rng(0xABCD_EF01_2345_6789 ^ sc.seed.wrapping_mul(0xD6E8_FEB8_6659_FD93)),
            max_dis: 0,
            max_settling_dis: 0,
            in_flight: 0,
            max_utc: 0,
            max_fleet_utc: 0,
            refused: 0,
            max_rel: 0,
            max_rel_slew: 0,
            slew_windows: 0,
            wall_back: 0,
            hour_start: vec![(0, 0); n],
            rate_errors: Vec::new(),
        }
    }

    /// 1. True time advances; every clock ticks at its own rate. A box whose wall crosses the
    ///    instant of its scheduled coordinated step inside this window applies it AT the crossing
    ///    (the controller polls `due` every loop iteration, 1 ms / 50 µs), so the landing instant
    ///    is resolved below the window.
    fn advance_clocks(&mut self, w: u64, t0_ns: f64) {
        let (gm_a_ppm, gm_b_ppm) = (GM_A_PPM, GM_B_PPM);
        let t_now_s = w as f64 * WINDOW_S;
        let grace = self.sc.grace;
        self.utc.advance(TRUE_DT_NS, self.sc.utc_vs_gm_ppm);
        for &(at, jump) in &self.sc.utc_jumps {
            if at == w {
                self.utc.ns += jump;
            }
        }
        self.gm_a.advance(TRUE_DT_NS, gm_a_ppm);
        self.gm_b_pre.advance(TRUE_DT_NS, gm_b_ppm);
        self.gm_b_post.advance(TRUE_DT_NS, gm_b_ppm);
        for b in self.boxes.iter_mut() {
            let wander =
                b.wander_ppm * (2.0 * std::f64::consts::PI * t_now_s / b.wander_period_s).sin();
            let rate = b.osc_ppm + wander + b.word_ppm;
            b.rate_ppm = rate;
            b.wander_now_ppm = wander;
            let scale = 1.0 + rate * 1e-6;
            // #119: the slew's rate term, switched on/off at its start/end instants inside the
            // window (the controller re-applies the word from its 1 ms loop at those instants).
            b.advance_slew(TRUE_DT_NS);
            let start = b.wall_ns();
            // The crossing is judged on the integer wall this window ENDS on (#119 follow-up: an
            // instant exactly at the end — the master's own announce lands on its window grid, and
            // the wall can advance exactly the lead — is reached in this window, as the
            // controller's `due` fires at `wall >= instant`; a float comparison missed it).
            let mut end = b.wall;
            end.advance(TRUE_DT_NS, rate);
            let end_wall = end.ns + b.stepped + b.slewed_ns;
            let crossing = b.follower.pending().and_then(|p| {
                let to_cross = ((p.effective_wall_ns - start) as f64 / scale).min(TRUE_DT_NS);
                (p.effective_wall_ns > start && p.effective_wall_ns <= end_wall)
                    .then_some((p, to_cross))
            });
            b.wall.advance(TRUE_DT_NS, rate);
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
    }

    /// 1b. #119: a slew that has paid its amount is folded into the anchor (`D` unchanged) — the
    ///     controller does it every loop iteration.
    fn fold_completed_slews(&mut self) {
        for b in self.boxes.iter_mut() {
            if let Some(anchor) = b.core.anchor_ns() {
                if let Some(fold) = b.follower.take_completed_slew(anchor, b.wall_ns()) {
                    b.core.set_anchor(anchor + fold);
                }
            }
        }
    }

    /// 2. A scheduled step whose instant is already behind the wall (scheduled at or after its
    ///    own instant) — applied at the boundary. Never expected; the checks reject it.
    fn apply_overdue_steps(&mut self, w: u64, t0_ns: f64) {
        let grace = self.sc.grace;
        for b in self.boxes.iter_mut() {
            if let Some(due) = b.follower.due(b.wall_ns()) {
                b.apply_step(
                    due.seq,
                    due.delta_ns,
                    StepKind::Late,
                    t0_ns + TRUE_DT_NS,
                    w,
                    grace,
                );
            }
        }
    }

    /// 3. One PTP window per box: median of noisy (t2 − t1) against the grandmaster it hears.
    ///    With `grace` the controller's 2 s post-step grace is modelled: those windows are dropped
    ///    and the word is held.
    fn ptp_windows(&mut self, w: u64) -> MasterWindow {
        let mut master = MasterWindow {
            rebase: None,
            ran: false,
        };
        let (gm_a, gm_b_pre, gm_b_post) = (&self.gm_a, &self.gm_b_pre, &self.gm_b_post);
        let t_end_ns = (w + 1) as f64 * TRUE_DT_NS;
        let judged = w > self.sc.settle_windows;
        for (i, b) in self.boxes.iter_mut().enumerate() {
            let (gm_id, gm) = gm_view(w, b.lag, gm_a, gm_b_pre, gm_b_post);
            let gm_ppm = if gm_id == 1 { GM_A_PPM } else { GM_B_PPM };
            // #119: every sample is de-slewed by the displacement the box's slew schedules at its
            // wall, so the phase lock never reads the deliberate slew as a phase error.
            let deslew = b.slew_displacement();
            let (rel_ppm, landing) = (b.rate_ppm - gm_ppm, b.last_landing);
            let mut samples: Vec<i64> = (0..SAMPLES_PER_WINDOW)
                .map(|_| {
                    let noise = b.rng.gauss() * PTP_NOISE_NS;
                    // #119 (1.11.1): a Windows box stamps each sample with its stepped clock at the
                    // capture instant, processed at the window's end (a queue that can span a
                    // step). An ideal box reads both clocks at the window's end.
                    let capture = b
                        .win
                        .as_mut()
                        .map_or(0, |win| win.capture_shift_ns(t_end_ns, rel_ppm, landing));
                    // wall_rx − gm_tx = (wall − gm) + delay, plus the timestamp noise.
                    b.wall_ns() - gm.ns + (b.delay_ns + noise).round() as i64 - deslew - capture
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
            let master_offline = i == 0 && self.sc.master_offline_at(w);
            if w < b.grace_until || master_offline {
                if master_offline && b.core.engaged() {
                    // No PTP, no phase lock: the controller's `on_ptp_offline_edge` holds the
                    // learned frequency through the free-run.
                    b.fresh = false;
                    b.core.disengage();
                    b.word_ppm = b.core.integrator_ppm();
                }
                b.words.push(b.word_ppm);
                b.audit_rate(gm_ppm, judged);
                continue;
            }
            let out = b.core.on_window(median, true, b.word_ppm, WINDOW_S);
            b.fresh = true;
            if i == 0 {
                master.ran = true;
            }
            if let AnchorEvent::Rebased { old_ns, new_ns } = out.event {
                b.rebases += 1;
                b.follower.rebase_slew(new_ns - old_ns);
                if i == 0 {
                    master.rebase = Some((old_ns, new_ns));
                }
            }
            b.word_ppm = out.freq_ppm.expect("locked from the start: always engaged");
            b.words.push(b.word_ppm);
            b.audit_rate(gm_ppm, judged);
        }
        master
    }

    /// 4. The master: authority lifecycle + UTC every 10 s (the controller's date_sync glue).
    fn master_cycle(&mut self, w: u64, t0_ns: f64, window: MasterWindow) {
        let grace = self.sc.grace;
        let offline = self.sc.master_offline_at(w);
        let m = &mut self.boxes[0];
        let anchor = m.core.anchor_ns().unwrap();
        let now_ptp = m.wall_ns() - anchor;
        if self.authority.is_none() {
            let a = DateAuthority::new(anchor, now_ptp, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS)
                .with_slew_ppm(self.sc.slew_ppm);
            let act = m.follower.on_announce(a.announce(), anchor, m.wall_ns());
            assert_eq!(
                act,
                FollowAction::None,
                "the authority is aligned with itself"
            );
            self.authority = Some(a);
        }
        let a = self.authority.as_mut().unwrap();
        if let Some((old_ns, new_ns)) = window.rebase {
            // The master's re-anchor on a new time base rebases the fleet offset (no step).
            let disp = m.follower.displacement_at_wall(new_ns, m.wall_ns());
            let seq_before = a.seq();
            master_rebases_fleet(a, m.wall_ns(), old_ns, new_ns, disp);
            // #119 follow-up: a step still pending is re-announced in the new base under the
            // rebase's seq (same wall instant, same size); a box that re-anchored first takes it
            // under that seq. With a micro step pending a quarter of the time, this now happens.
            if a.pending_step_ns(m.wall_ns() - m.d_in_effect()).is_some() {
                self.renamed_steps.push((seq_before, a.seq()));
            }
        }
        if window.ran {
            self.snapshot = Some(master_publishes(m, a)); // the status write ending the window
        }
        if !offline {
            let before = m.steps.len();
            master_reconcile(m, a, t0_ns + TRUE_DT_NS, w, grace);
            if m.steps.len() != before {
                self.snapshot = Some(master_publishes(m, a)); // `apply_date_step`
            }
        }
        // #119 follow-up: the micro-correction clock, every loop iteration.
        if let Some(fed) = master_tick_authority(m, a, offline) {
            Self::master_takes(
                m,
                fed,
                w,
                &mut self.announced,
                &mut self.announced_slews,
                &mut self.corrections,
            );
            self.snapshot = Some(master_publishes(m, a)); // published at once
        }
        if w % NTP_INTERVAL_WINDOWS == 0 && w > 0 {
            let err = self.utc.ns - m.wall_ns() + self.sc.ntp_noise.sample(&mut self.ntp_rng);
            if let Some(fed) = master_feed_authority(m, a, err, offline) {
                Self::master_takes(
                    m,
                    fed,
                    w,
                    &mut self.announced,
                    &mut self.announced_slews,
                    &mut self.corrections,
                );
            }
            if master_publishes_after_ntp(offline) {
                self.snapshot = Some(master_publishes(m, a));
            }
            if offline {
                // Its OWN wall: the local NTP date path (the legacy step gate, two agreeing
                // over-threshold readings).
                let over = err.abs() > MASTER_LOCAL_THRESHOLD_NS;
                if over
                    && self
                        .master_local_candidate
                        .is_some_and(|c: i64| c.signum() == err.signum())
                {
                    master_local_step(m, a, err);
                    if MASTER_PUBLISHES_AFTER_LOCAL_STEP {
                        self.snapshot = Some(master_publishes(m, a));
                    }
                    self.master_local_candidate = None;
                } else {
                    self.master_local_candidate = over.then_some(err);
                }
            }
        }
        // The 10 s `tick_status`, on its own timer: not in phase with the NTP cadence (here 6.5 s
        // after it, i.e. later than the 5 s announce lead).
        if w % 20 == 13 {
            self.snapshot = Some(master_publishes(m, a));
        }
        if w > self.sc.settle_windows && !self.sc.settling_after_a_utc_jump(w) {
            self.max_utc = self.max_utc.max((self.utc.ns - m.wall_ns()).abs());
        }
    }

    /// The master's glue for an announce its authority made: recorded, and scheduled by its own
    /// scheduler when it is on the fleet line — where D and the authority agree to the ns, so a
    /// step or a slew is always scheduled (the controller's `master_schedules_own`).
    fn master_takes(
        m: &mut Box_,
        (ann, own, before): (DateAnnounce, bool, i64),
        w: u64,
        announced: &mut Vec<(u32, i64)>,
        announced_slews: &mut Vec<(u32, i64)>,
        corrections: &mut Vec<(u64, i64)>,
    ) {
        match ann.as_slew() {
            Some(sl) => {
                announced_slews.push((ann.seq, sl.amount_ns()));
                corrections.push((w, sl.amount_ns()));
            }
            None => {
                announced.push((ann.seq, ann.date_offset_ns - before));
                corrections.push((w, ann.date_offset_ns - before));
            }
        }
        if own {
            let anchor = m.core.anchor_ns().unwrap();
            match m.follower.on_announce(ann, anchor, m.wall_ns()) {
                FollowAction::Scheduled { .. } | FollowAction::SlewScheduled { .. } => {}
                other => panic!("master schedules its own step / slew: {other:?}"),
            }
        }
    }

    /// 5. Followers poll the master's 31900 every second (10 % of polls lost). The reply carries
    ///    the master's wall, its published D and the D's grandmaster (the extension); a follower
    ///    adopts it only in its own time base — the controller's exact checks.
    fn follower_polls(&mut self, w: u64, t0_ns: f64) {
        if w % POLL_INTERVAL_WINDOWS != 0 {
            return;
        }
        let grace = self.sc.grace;
        let published = self.snapshot.expect("published since the first window");
        let master_wall_now = self.boxes[0].wall_ns();
        let ann = DateAnnounce {
            date_offset_ns: published.date_offset_ns,
            effective_ptp_ns: published.effective_ptp_ns,
            seq: published.seq,
            slew: published.slew,
            micro: published.micro,
        };
        for b in self.boxes.iter_mut().skip(1) {
            if self.net_rng.uniform() < POLL_LOSS {
                continue;
            }
            if !follower_accepts(b, &published, master_wall_now) {
                self.refused += 1;
                continue;
            }
            let anchor = b.core.anchor_ns().expect("accepted ⇒ anchored");
            match b.follower.on_announce(ann, anchor, b.wall_ns()) {
                FollowAction::None
                | FollowAction::Scheduled { .. }
                | FollowAction::SlewScheduled { .. } => {}
                FollowAction::Absorb { new_anchor_ns } => b.core.set_anchor(new_anchor_ns),
                FollowAction::Step { delta_ns, kind } => {
                    b.apply_step(ann.seq, delta_ns, kind, t0_ns + TRUE_DT_NS, w, grace)
                }
            }
        }
    }

    /// 6a. Raw wall disagreement at the same true instant, INCLUDING each box's own PTP path
    ///     delay (a box holds `t2 − t1 = D`, so its wall sits `delay` behind the GM line) — exactly
    ///     what a cross-box genlock grid sees.
    fn measure_disagreement(&mut self, w: u64, t0_ns: f64) {
        if w > self.sc.settle_windows && !self.sc.settling_after_a_utc_jump(w) {
            self.max_fleet_utc = self
                .max_fleet_utc
                .max((self.utc.ns - self.boxes[1].wall_ns()).abs());
        }
        if w <= ALL_JOINED_AFTER {
            return;
        }
        // A coordinated step is IN FLIGHT at this boundary when some box landed it in this window
        // while another still holds it pending (their walls straddle its instant by µs).
        let landed_now = |b: &Box_| -> Vec<u32> {
            b.steps
                .iter()
                .filter(|s| s.2 == StepKind::Coordinated && s.3 >= t0_ns)
                .map(|s| s.0)
                .collect()
        };
        // The master is judged against the fleet only while it is ON the fleet line (PTP online
        // and its D equal to the authority's): during its own PTP outage it runs the local NTP
        // date path by design.
        let a = self.authority.as_ref().unwrap();
        let m = &self.boxes[0];
        let m_d = m.d_in_effect();
        let master_on_line =
            !self.sc.master_offline_at(w) && a.in_effect_ns(m.wall_ns() - m_d) == m_d;
        let slewing = a.slew_in_progress(m.wall_ns() - m_d).is_some()
            || self.boxes.iter().any(|b| b.follower.held_slew().is_some());
        if slewing && (w == GM_CHANGE_AT_WINDOW || w == GM_REBOOT_AT_WINDOW) {
            self.gm_events_in_slew += 1;
        }
        let judged: Vec<&Box_> = self
            .boxes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 || master_on_line)
            .map(|(_, b)| b)
            .collect();

        // (#119 follow-up: a step a grandmaster rebase re-announced is pending under the rebase's
        // seq on a box that re-anchored first, while the master lands it under the original one.)
        let original_seq = |seq: u32| {
            self.renamed_steps
                .iter()
                .find(|(_, new)| *new == seq)
                .map_or(seq, |(old, _)| *old)
        };
        // Both sides in the ORIGINAL seq: a box may land the step under either.
        let landed: Vec<u32> = judged
            .iter()
            .flat_map(|b| landed_now(b))
            .map(original_seq)
            .collect();
        let straddling = judged.iter().any(|b| {
            b.follower
                .pending()
                .is_some_and(|p| landed.contains(&original_seq(p.seq)))
        });
        // The documented double fault: when the master returns on a new grandmaster it re-bases
        // the fleet D with its own free-run error (~100 µs here). Followers take it at their next
        // poll — a step above the 100 µs absorb tolerance, else an absorb that the phase lock
        // slews out over ~2 minutes. That settling is the fault's cost, not a steady-state
        // disagreement: it is bounded on its own (`max_settling_dis`, and ≤ one late step < 1 ms
        // per follower).
        let sc = self.sc;
        let double_fault_settling = sc.gm_change_in_master_outage
            && sc.master_ptp_offline.iter().any(|&(from, to)| {
                (from..to).contains(&GM_CHANGE_AT_WINDOW) && (to..to + 600).contains(&w)
            });
        let hi = judged.iter().map(|b| b.wall_ns()).max().unwrap();
        let lo = judged.iter().map(|b| b.wall_ns()).min().unwrap();
        if straddling {
            self.in_flight += 1;
        } else if double_fault_settling {
            self.max_settling_dis = self.max_settling_dis.max(hi - lo);
        } else {
            self.max_dis = self.max_dis.max(hi - lo);
            // #119: the relative phase — each wall plus its own path delay (a box holds its wall
            // `delay` behind the GM line, see above).
            let rel: Vec<i64> = judged
                .iter()
                .map(|b| b.wall_ns() + b.delay_ns.round() as i64)
                .collect();
            let spread = rel.iter().max().unwrap() - rel.iter().min().unwrap();
            self.max_rel = self.max_rel.max(spread);
            if slewing {
                self.max_rel_slew = self.max_rel_slew.max(spread);
                self.slew_windows += 1;
            }
        }
        // #119: no wall ever reads less than at the previous boundary.
        for b in self.boxes.iter_mut() {
            let now = b.wall_ns();
            if now < b.last_wall {
                self.wall_back += 1;
            }
            b.last_wall = now;
        }
    }

    /// 6b. Hourly effective rate vs the grandmaster each box hears (hours around a grandmaster
    ///     event mix two rates / two bases by construction and are skipped).
    fn audit_rates(&mut self, w: u64) {
        if w % 7_200 != 0 {
            return;
        }
        let near_event = [GM_CHANGE_AT_WINDOW, GM_REBOOT_AT_WINDOW]
            .iter()
            .any(|&e| (w.saturating_sub(7_200)..w).contains(&e) || (e..e + 8).contains(&w));
        for (i, b) in self.boxes.iter().enumerate() {
            let (_, gm) = gm_view(w, b.lag, &self.gm_a, &self.gm_b_pre, &self.gm_b_post);
            let (w0, g0) = self.hour_start[i];
            // The master's hours around its own PTP outage are skipped too: with no PTP it
            // free-runs on its held word by design (it has nothing to lock to).
            let master_outage = i == 0
                && self
                    .sc
                    .master_ptp_offline
                    .iter()
                    .any(|&(from, to)| from < w && to + 600 > w.saturating_sub(7_200));
            if w > 7_200 && !near_event && !master_outage {
                // The continuous clock (the wall without its steps) against the GM's time.
                let d_wall = (b.wall.ns - w0) as f64;
                let d_gm = (gm.ns - g0) as f64;
                self.rate_errors.push((d_wall / d_gm - 1.0) * 1e6);
            }
            self.hour_start[i] = (b.wall.ns, gm.ns);
        }
    }

    fn into_result(mut self) -> RunResult {
        // #119 follow-up: an announce whose instant is still ahead when the run ends (at a step
        // every 20 s, the last one often is) was taken by no box yet: it is not judged.
        let m = &self.boxes[0];
        let now_ptp = m.wall_ns() - m.d_in_effect();
        if let Some(a) = self.authority.as_ref() {
            if a.pending_step_ns(now_ptp).is_some()
                && self.announced.last().map(|l| l.0) == Some(a.seq())
            {
                self.announced.pop();
            }
        }
        RunResult {
            words: self.boxes.iter().map(|b| b.words.clone()).collect(),
            steps: self.boxes.iter().map(|b| b.steps.clone()).collect(),
            max_disagreement_ns: self.max_dis,
            max_settling_disagreement_ns: self.max_settling_dis,
            in_flight_samples: self.in_flight,
            max_utc_error_ns: self.max_utc,
            max_fleet_utc_error_ns: self.max_fleet_utc,
            rate_errors_ppm: self.rate_errors,
            rebases: self.boxes.iter().map(|b| b.rebases).collect(),
            late_steps: self.boxes.iter().map(|b| b.follower.late_steps()).collect(),
            announced: self.announced,
            announced_slews: self.announced_slews,
            renamed_steps: self.renamed_steps,
            refused_replies: self.refused,
            max_relative_phase_ns: self.max_rel,
            max_relative_phase_in_slew_ns: self.max_rel_slew,
            slew_windows: self.slew_windows,
            wall_went_back: self.wall_back,
            corrections: self.corrections,
            gm_events_in_slew: self.gm_events_in_slew,
            master_catch_ups: self.boxes[0].catch_ups,
            rate_audits: self.boxes.iter().map(|b| b.rate_audit).collect(),
            max_step_residual_ns: self
                .boxes
                .iter()
                .map(|b| b.win.as_ref().map_or(0, |win| win.max_residual_ns))
                .collect(),
        }
    }
}

fn run(sc: &Scenario) -> RunResult {
    let mut bench = Bench::new(sc);
    for w in 0..sc.run_windows {
        let t0_ns = w as f64 * TRUE_DT_NS;
        let master_steps_at_start = bench.boxes[0].steps.len();
        bench.advance_clocks(w, t0_ns);
        bench.fold_completed_slews();
        bench.apply_overdue_steps(w, t0_ns);
        if bench.boxes[0].steps.len() != master_steps_at_start {
            if let Some(a) = bench.authority.as_ref() {
                bench.snapshot = Some(master_publishes(&bench.boxes[0], a)); // `apply_date_step`
            }
        }
        let master_window = bench.ptp_windows(w);
        bench.master_cycle(w, t0_ns, master_window);
        bench.follower_polls(w, t0_ns);
        bench.measure_disagreement(w, t0_ns);
        bench.audit_rates(w);
    }
    bench.into_result()
}

// A crate root resolves `mod x;` beside itself (`tests/x.rs`), which cargo would also build as a
// test target of its own: the path keeps the scenarios inside this bench.
#[path = "two_clock_bench/scenarios.rs"]
mod scenarios;
