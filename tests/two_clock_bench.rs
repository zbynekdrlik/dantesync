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
}

impl Scenario {
    fn plain(label: &'static str, utc_vs_gm_ppm: f64, grace: bool) -> Self {
        Scenario {
            label,
            utc_vs_gm_ppm,
            grace,
            master_boot_err_ns: 0,
            master_ptp_offline: Vec::new(),
            gm_change_in_master_outage: false,
        }
    }
    fn master_offline_at(&self, w: u64) -> bool {
        self.master_ptp_offline
            .iter()
            .any(|&(from, to)| (from..to).contains(&w))
    }
}

// ----------------------------------------------------------------------------------------------
// The controller glue this bench mirrors (`src/controller/date_sync.rs`). Kept in these few
// functions so a change to the controller's glue has exactly one place to be mirrored.
// ----------------------------------------------------------------------------------------------

/// The master's published STATUS snapshot (`publish_date_status`): the FLEET D (its authority's
/// announce, even while its own wall is off the fleet line), and its own D in effect. It is only
/// refreshed at the controller's publish points (a PTP window, an NTP cycle, a step, the 10 s
/// tick), and the time server reads it at REPLY time — so a stale snapshot is visible here.
#[derive(Clone, Copy)]
struct Published {
    date_offset_ns: i64,
    effective_ptp_ns: i64,
    seq: u32,
    gm: u8,
    master_anchor_ns: i64,
}

fn master_publishes(m: &Box_, a: &DateAuthority) -> Published {
    let ann = a.announce();
    Published {
        date_offset_ns: ann.date_offset_ns,
        effective_ptp_ns: ann.effective_ptp_ns,
        seq: ann.seq,
        gm: m.core_gm,
        master_anchor_ns: m.core.anchor_ns().expect("anchored"),
    }
}

/// Whether an NTP cycle refreshes the master's published status (`ntp_under_date_authority`):
/// always — also off line, or an announce would wait for the next 10 s tick.
fn master_publishes_after_ntp(_ptp_offline: bool) -> bool {
    true
}

/// Whether the master's local NTP step refreshes it (`note_local_date_step`).
const MASTER_PUBLISHES_AFTER_LOCAL_STEP: bool = true;

/// A follower's applicability checks (`service_date_offset`); the time server computes the
/// replier's PTP now from the SNAPSHOT's D in effect and the live wall.
fn follower_accepts(b: &Box_, p: &Published, master_wall_now: i64) -> bool {
    let Some(anchor) = b.core.anchor_ns() else {
        return false;
    };
    let now_ptp = master_wall_now - p.master_anchor_ns;
    !b.core.rebase_pending() && b.core_gm == p.gm && same_time_base(now_ptp, b.wall_ns(), anchor)
}

/// The master's local NTP step while it has no PTP (`note_local_date_step`): its own wall and D
/// only — the fleet D never moves for one box's fault.
fn master_local_step(m: &mut Box_, _a: &mut DateAuthority, delta_ns: i64) {
    m.stepped += delta_ns;
    m.core.note_step(delta_ns);
    m.fresh = false;
    m.follower.cancel_pending();
}

/// The master's UTC reading → the authority (`ntp_under_date_authority`): the FLEET line's error
/// (`reading + anchor − fleet`), fed also while the master has no PTP or is off the line, so the
/// fleet stays on UTC. Returns the announce, whether the master schedules it for its own wall
/// (only on the line), and the fleet D it replaces.
fn master_feed_authority(
    m: &Box_,
    a: &mut DateAuthority,
    utc_err_ns: i64,
    ptp_offline: bool,
) -> Option<(DateAnnounce, bool, i64)> {
    let anchor = m.core.anchor_ns().expect("anchored");
    let now_ptp = m.wall_ns() - anchor;
    let fleet = a.in_effect_ns(now_ptp);
    let fleet_err = utc_err_ns + (anchor - fleet);
    let on_line = anchor == fleet && !ptp_offline;
    a.on_utc_error(fleet_err, now_ptp)
        .map(|ann| (ann, on_line, fleet))
}

/// The master re-anchored on a new time base (`handle_phase_anchor_event`): the fleet D moves by
/// the observed base shift, never onto the master's own (possibly off-line) anchor.
fn master_rebases_fleet(a: &mut DateAuthority, master_wall_ns: i64, old_ns: i64, new_ns: i64) {
    let now_ptp_old = master_wall_ns - old_ns;
    let fleet_old = a.in_effect_ns(now_ptp_old);
    a.rebase(fleet_old + (new_ns - old_ns), now_ptp_old);
}

/// The master's own re-alignment to the fleet line once its PTP is back
/// (`realign_master_to_fleet`): one Join step that lands its wall ON the fleet line (removing the
/// phase error the outage left, measured by a window taken after PTP came back), nothing while a
/// step is pending.
fn master_reconcile(m: &mut Box_, a: &DateAuthority, t_ns: f64, w: u64, grace: bool) {
    // (The controller also gates on its step-failure backoff; the bench's clocks never fail.)
    if !m.core.engaged() || m.core.rebase_pending() {
        return;
    }
    let anchor = m.core.anchor_ns().expect("anchored");
    let now_ptp = m.wall_ns() - anchor;
    if a.pending_step_ns(now_ptp).is_some() || m.follower.pending().is_some() {
        return;
    }
    let fleet = a.in_effect_ns(now_ptp);
    if fleet == anchor || !m.fresh {
        return;
    }
    let e = m.core.last_error_ns().unwrap_or(0);
    let delta = fleet - anchor - e;
    if delta.abs() > dantesync::date_offset::ABSORB_TOLERANCE_NS {
        m.apply_step(a.seq(), delta, StepKind::Join, t_ns, w, grace);
    }
    m.core.set_anchor(fleet);
}

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
    /// A phase-lock window ran since the last step / PTP outage (the controller's `fresh_window`).
    fresh: bool,
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
                // The rate servo hands over a word that is 0.4 ppm off the truth.
                word_ppm: -oscs[i] + 0.4,
                core: PhaseLockCore::new(),
                follower: DateFollower::new(),
                core_gm: 1,
                lag: (change_lags[i], reboot_lags[i]),
                grace_until: 0,
                fresh: false,
                rng: Rng(0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1) * 0x1000_0000_01B3),
                words: Vec::new(),
                steps: Vec::new(),
                rebases: 0,
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
            ntp_rng: Rng(0xD1B5_4A32_D192_ED03),
            net_rng: Rng(0xABCD_EF01_2345_6789),
            max_dis: 0,
            max_settling_dis: 0,
            in_flight: 0,
            max_utc: 0,
            max_fleet_utc: 0,
            refused: 0,
            hour_start: vec![(0, 0); n],
            rate_errors: Vec::new(),
        }
    }

    /// 1. True time advances; every clock ticks at its own rate. A box whose wall crosses the
    ///    instant of its scheduled coordinated step inside this window applies it AT the crossing
    ///    (the controller polls `due` every loop iteration, 1 ms / 50 µs), so the landing instant
    ///    is resolved below the window.
    fn advance_clocks(&mut self, w: u64, t0_ns: f64) {
        let (gm_a_ppm, gm_b_ppm) = (0.0, 3.0);
        let t_now_s = w as f64 * WINDOW_S;
        let grace = self.sc.grace;
        self.utc.advance(TRUE_DT_NS, self.sc.utc_vs_gm_ppm);
        self.gm_a.advance(TRUE_DT_NS, gm_a_ppm);
        self.gm_b_pre.advance(TRUE_DT_NS, gm_b_ppm);
        self.gm_b_post.advance(TRUE_DT_NS, gm_b_ppm);
        for b in self.boxes.iter_mut() {
            let wander =
                b.wander_ppm * (2.0 * std::f64::consts::PI * t_now_s / b.wander_period_s).sin();
            let rate = b.osc_ppm + wander + b.word_ppm;
            let scale = 1.0 + rate * 1e-6;
            let start = b.wall_ns();
            let crossing = b.follower.pending().and_then(|p| {
                let to_cross = (p.effective_wall_ns - start) as f64 / scale;
                (p.effective_wall_ns > start && to_cross <= TRUE_DT_NS).then_some((p, to_cross))
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
        for (i, b) in self.boxes.iter_mut().enumerate() {
            let (gm_id, gm) = gm_view(w, b.lag, gm_a, gm_b_pre, gm_b_post);
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
                continue;
            }
            let out = b.core.on_window(median, true, b.word_ppm, WINDOW_S);
            b.fresh = true;
            if i == 0 {
                master.ran = true;
            }
            if let AnchorEvent::Rebased { old_ns, new_ns } = out.event {
                b.rebases += 1;
                if i == 0 {
                    master.rebase = Some((old_ns, new_ns));
                }
            }
            b.word_ppm = out.freq_ppm.expect("locked from the start: always engaged");
            b.words.push(b.word_ppm);
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
            let a = DateAuthority::new(anchor, now_ptp, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS);
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
            master_rebases_fleet(a, m.wall_ns(), old_ns, new_ns);
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
        if w % NTP_INTERVAL_WINDOWS == 0 && w > 0 {
            let err =
                self.utc.ns - m.wall_ns() + (self.ntp_rng.gauss() * NTP_NOISE_NS).round() as i64;
            let fed = master_feed_authority(m, a, err, offline);
            if let Some((ann, own, before)) = fed {
                self.announced.push((ann.seq, ann.date_offset_ns - before));
                if own {
                    let anchor = m.core.anchor_ns().unwrap();
                    let act = m.follower.on_announce(ann, anchor, m.wall_ns());
                    assert!(
                        matches!(act, FollowAction::Scheduled { .. }),
                        "master schedules its own step: {act:?}"
                    );
                }
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
        if w > 240 {
            self.max_utc = self.max_utc.max((self.utc.ns - m.wall_ns()).abs());
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
                FollowAction::None | FollowAction::Scheduled { .. } => {}
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
        if w > 240 {
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
        let m_anchor = m.core.anchor_ns().unwrap();
        let master_on_line =
            !self.sc.master_offline_at(w) && a.in_effect_ns(m.wall_ns() - m_anchor) == m_anchor;
        let judged: Vec<&Box_> = self
            .boxes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 || master_on_line)
            .map(|(_, b)| b)
            .collect();
        let landed: Vec<u32> = judged.iter().flat_map(|b| landed_now(b)).collect();
        let straddling = judged.iter().any(|b| {
            b.follower
                .pending()
                .is_some_and(|p| landed.contains(&p.seq))
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

    fn into_result(self) -> RunResult {
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
            refused_replies: self.refused,
        }
    }
}

fn run(sc: &Scenario) -> RunResult {
    let mut bench = Bench::new(sc);
    for w in 0..HOURS * 3600 * 2 {
        let t0_ns = w as f64 * TRUE_DT_NS;
        let master_steps_at_start = bench.boxes[0].steps.len();
        bench.advance_clocks(w, t0_ns);
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
        self.fresh = false;
        self.steps.push((seq, delta_ns, kind, t_ns));
        if grace {
            self.grace_until = w + 1 + GRACE_WINDOWS;
        }
    }
}

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
        !r.announced.is_empty(),
        "[{label}] the authority never announced a date step"
    );
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
    let sc_plus = Scenario::plain("UTC +8 ppm vs GM", 8.0, false);
    let sc_minus = Scenario::plain("UTC -15 ppm vs GM", -15.0, false);
    let plus = run(&sc_plus);
    check(&sc_plus, &plus);
    let minus = run(&sc_minus);
    check(&sc_minus, &minus);

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
    assert_ne!(plus.announced.len(), minus.announced.len());
    let dir = |r: &RunResult| r.announced.iter().map(|s| s.1.signum()).sum::<i64>();
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
    };
    let r = run(&sc);
    check(&sc, &r);
}
