//! dantesync#126 — the bench's master RESTART, its followers' authority loss, and the
//! coordinated step on request.
//!
//! The master's PROCESS stops (no PTP window, no authority, nothing published, its NTP server
//! gone), starts a few seconds later (its NTP server serves its wall again) and is PTP-locked a
//! minute after that (strih-lx on 30.9.2026: 65 s from the restart to the authority heard again).
//! While it is down the kernel keeps the frequency the process restored on its way out, modelled
//! as none (the raw oscillator); from its start the rate servo holds the bench's hand-over word.
//!
//! - **1.15** (`restore`): no boot step; at its first lock the master restores the saved state
//!   (the controller's `restore_date_authority`, mirrored by `master_restores_authority`) and
//!   re-joins ITS OWN wall; the followers HOLD the fleet D through the gap (900 s by default).
//! - **1.14** (the negative control): the boot step to UTC, a new session at seq 1, and the
//!   followers back on their own NTP step path 30 s after the master went silent — each reads the
//!   master's stepped wall on its own cadence and steps at its own instant (dev1: +247.77 ms at
//!   06:02:42Z, resolume 06:02:42.272Z, cam7 06:02:46.318Z).
//!
//! The step on request (`DateAuthority::step_now`) is taken by the master on its own wall and
//! published at once; every follower schedules it from its poll and lands it at the instant.

use super::*;

/// A follower drops an authority it has not heard for this long: the controller's
/// `AUTHORITY_LOSS` (30 s).
pub(super) const AUTHORITY_LOSS_WINDOWS: u64 = 60;
/// The controller's default hold, `system.date_offset.authority_hold_s` = 900 s.
pub(super) const DEFAULT_HOLD_WINDOWS: u64 = 1_800;
/// A follower's local NTP date path reads the master's NTP server over the LAN …
const FOLLOWER_NTP_NOISE_NS: f64 = 50_000.0;
/// … and steps two agreeing readings beyond its threshold (the client's adaptive threshold read
/// 2.1–2.7 ms on dev1 at the incident).
const FOLLOWER_LOCAL_THRESHOLD_NS: i64 = 2 * MS;

/// The master's restart.
#[derive(Clone, Copy)]
pub(super) struct MasterRestart {
    /// The process stops at this window …
    pub(super) at: u64,
    /// … starts `gap` windows later (its NTP server answers again) …
    pub(super) gap: u64,
    /// … and is PTP-locked `acq` windows after that (its authority is back).
    pub(super) acq: u64,
    /// 1.15: restore the saved state; 1.14: the boot step to UTC and a new session.
    pub(super) restore: bool,
}

/// What the restart did.
#[derive(Clone, Debug, Default)]
pub(super) struct RestartLog {
    /// The authority's seq when the master stopped, and its first after the restart.
    pub(super) seq_before: Option<u32>,
    pub(super) seq_after: Option<u32>,
    /// The saved state was restored.
    pub(super) restored: bool,
    /// The boot step the 1.14 path took (UTC − wall at the start).
    pub(super) boot_step_ns: Option<i64>,
    /// The seqs of the steps on request, as accepted.
    pub(super) request_seqs: Vec<u32>,
}

impl Bench<'_> {
    /// The restart's events, at the start of window `w`.
    pub(super) fn restart_events(&mut self, w: u64) {
        let Some(r) = self.sc.master_restart else {
            return;
        };
        if w == r.at {
            self.restart_log.seq_before = self.authority.as_ref().map(|a| a.seq());
            self.master_down = true;
            self.authority = None;
            self.snapshot = None;
            let m = &mut self.boxes[0];
            m.core = PhaseLockCore::new();
            m.follower = DateFollower::new();
            m.fresh = false;
            m.realign_after_outage = false;
            m.grace_until = 0;
            m.word_ppm = 0.0;
        } else if w == r.at + r.gap {
            let err =
                self.utc.ns - self.boxes[0].wall_ns() + self.sc.ntp_noise.sample(&mut self.ntp_rng);
            let m = &mut self.boxes[0];
            m.word_ppm = -m.osc_ppm + 0.4;
            // 1.14: `run_ntp_sync` steps the wall to UTC (unbounded, before any anchor).
            // RED stub (#126): the pre-1.15 start always takes the boot step.
            if err.abs() > 50 * MS {
                m.stepped += err;
                self.restart_log.boot_step_ns = Some(err);
            }
        } else if w == r.at + r.gap + r.acq {
            // The first lock: the next PTP window anchors, `master_cycle` builds the authority.
            self.master_down = false;
        }
    }

    /// The master's NTP server answers (its process runs) at window `w`.
    fn master_ntp_up(&self, w: u64) -> bool {
        self.sc
            .master_restart
            .map_or(true, |r| !(r.at..r.at + r.gap).contains(&w))
    }

    /// The followers' authority loss (the controller's `hold_or_forget_silent_authority`): a
    /// follower that has not heard an applicable reply for the loss window plus its hold forgets
    /// the authority, and then runs its local NTP date path against the master's NTP server
    /// (two agreeing readings beyond the threshold step its wall, `D` moving with it) until it
    /// hears the authority again. No-op without the loss model.
    pub(super) fn follower_authority_loss(&mut self, w: u64) {
        let Some(_hold) = self.sc.follower_hold_windows else {
            return;
        };
        // RED stub (#126): the pre-1.15 follower has no hold (the fallback at the 30 s loss).
        let hold = 0;
        let ntp_up = self.master_ntp_up(w);
        let master_wall = self.boxes[0].wall_ns();
        let grace = self.sc.grace;
        for (i, b) in self.boxes.iter_mut().enumerate().skip(1) {
            if b.follower.adopted() {
                let silent = w.saturating_sub(b.last_heard_w.unwrap_or(0));
                if silent > AUTHORITY_LOSS_WINDOWS + hold {
                    b.follower.forget();
                    b.forgotten = true;
                }
                continue;
            }
            // Each follower reads on its own 10 s cadence (the incident's scattered instants).
            if !b.forgotten || !ntp_up || (w + 3 * i as u64) % NTP_INTERVAL_WINDOWS != 0 {
                continue;
            }
            let noise = (self.ntp_rng.gauss() * FOLLOWER_NTP_NOISE_NS).round() as i64;
            let err = master_wall - b.wall_ns() + noise;
            let over = err.abs() > FOLLOWER_LOCAL_THRESHOLD_NS;
            if over
                && b.local_candidate
                    .is_some_and(|c| c.signum() == err.signum())
            {
                b.stepped += err;
                b.core.note_step(err);
                b.fresh = false;
                b.local_steps.push((w, err));
                if grace {
                    b.grace_until = w + 1 + GRACE_WINDOWS;
                }
                b.local_candidate = None;
            } else {
                b.local_candidate = over.then_some(err);
            }
        }
    }
}

/// A step on request 1 h in (the authority's seq is 2 from then), the master restarts at 2 h (63 ms
/// of UTC drift since that step at +17.6 ppm), is down 2 s and locked 65 s later; another step on
/// request at 3 h. Daily mode (the default), no grandmaster event in the 4 h.
const FIRST_REQUEST_AT: u64 = 3_600 * 2;
const RESTART_AT: u64 = 2 * 3_600 * 2;
const RESTART_GAP: u64 = 4;
const RESTART_ACQ: u64 = 130;
const REQUEST_AT: u64 = 3 * 3_600 * 2;

fn restart_scenario(label: &'static str, restore: bool, hold_windows: u64) -> Scenario {
    let mut sc = Scenario::plain(label, 17.6, true);
    sc.correction = CorrectionMode::Daily(DailyConfig::default());
    sc.gm_b_ppm = GM_A_PPM;
    sc.run_windows = 4 * 3_600 * 2;
    sc.master_restart = Some(MasterRestart {
        at: RESTART_AT,
        gap: RESTART_GAP,
        acq: RESTART_ACQ,
        restore,
    });
    sc.follower_hold_windows = Some(hold_windows);
    sc.step_requests_at = vec![FIRST_REQUEST_AT, REQUEST_AT];
    sc
}

/// True time (s) of a window.
fn at_s(w: u64) -> f64 {
    w as f64 * WINDOW_S
}

#[test]
fn a_master_restart_keeps_the_fleet_date_and_a_step_on_request_lands_everywhere_at_once_126() {
    let sc = restart_scenario("restart: 1.15 (restore + hold)", true, DEFAULT_HOLD_WINDOWS);
    let r = run(&sc);
    let rl = &r.restart;
    let restart_ns = at_s(RESTART_AT) * 1e9;
    println!(
        "[{}] {rl:?}; master steps {:?}; max wall disagreement {} µs ({} µs while the master \
         re-acquires), relative phase {} µs",
        sc.label,
        r.steps[0],
        r.max_disagreement_ns / US,
        r.max_settling_disagreement_ns / US,
        r.max_relative_phase_ns / US
    );
    // The master restored its saved state: the same session, no boot step.
    assert!(rl.restored, "the saved state was restored");
    assert_eq!(rl.seq_before, Some(2), "the step at 1 h made seq 2");
    assert_eq!(rl.seq_after, Some(2), "the same session after the restart");
    assert_eq!(rl.boot_step_ns, None, "no boot step");
    assert_eq!(rl.request_seqs, vec![2, 3], "the session continues");
    // Its own wall re-joined the fleet line alone: at most one Join, the gap's free-run.
    let master_joins: Vec<&Step> = r.steps[0]
        .iter()
        .filter(|s| s.2 == StepKind::Join && s.3 > restart_ns)
        .collect();
    assert!(
        master_joins.len() <= 1 && master_joins.iter().all(|s| s.1.abs() < MS),
        "the master's own re-join: {master_joins:?}"
    );
    // No follower stepped around the restart: after its join its ONLY steps are the two requested.
    for (i, steps) in r.steps.iter().enumerate().skip(1) {
        let after_join: Vec<(u32, StepKind)> = steps
            .iter()
            .filter(|s| s.3 > 10e9)
            .map(|s| (s.0, s.2))
            .collect();
        assert_eq!(
            after_join,
            vec![(2, StepKind::Coordinated), (3, StepKind::Coordinated)],
            "box {i} stepped around the restart"
        );
        assert!(
            r.local_steps[i].is_empty(),
            "box {i} took the local NTP path: {:?}",
            r.local_steps[i]
        );
    }
    assert!(r.late_steps.iter().all(|&l| l == 0), "{:?}", r.late_steps);
    // Each step on request: ONE coordinated step of the current error on every box, the same
    // size, landing at the same instant — the one after the restart too.
    for (seq, expected) in [
        (2, accrued_ns(17.6, at_s(FIRST_REQUEST_AT) + 10.0)),
        (3, accrued_ns(17.6, at_s(REQUEST_AT - FIRST_REQUEST_AT))),
    ] {
        let landings: Vec<(i64, f64)> = r
            .steps
            .iter()
            .filter_map(|steps| {
                steps
                    .iter()
                    .find(|s| s.0 == seq && s.2 == StepKind::Coordinated)
                    .map(|s| (s.1, s.3))
            })
            .collect();
        assert_eq!(landings.len(), 6, "every box took seq {seq}: {landings:?}");
        assert!(
            landings.iter().all(|l| l.0 == landings[0].0),
            "{landings:?}"
        );
        assert!(
            (landings[0].0 - expected).abs() < 5 * MS,
            "seq {seq}: the whole UTC error: {} vs {expected}",
            landings[0].0
        );
        let t: Vec<f64> = landings.iter().map(|l| l.1).collect();
        let spread =
            t.iter().cloned().fold(f64::MIN, f64::max) - t.iter().cloned().fold(f64::MAX, f64::min);
        println!(
            "[{}] step on request seq {seq}: {} ms landed across {:.1} µs",
            sc.label,
            landings[0].0 as f64 / 1e6,
            spread / 1e3
        );
        assert!(spread < 100_000.0, "seq {seq} landed across {spread} ns");
    }
    // The fleet never split: every wall within 100 µs at every instant, within 50 µs of relative
    // phase; while the restarted master re-acquires (its absorbed free-run, pulled in by its phase
    // lock) within the settling bound.
    assert!(
        r.max_disagreement_ns < 100 * US,
        "walls disagreed by {} µs",
        r.max_disagreement_ns / US
    );
    assert!(
        r.max_settling_disagreement_ns < SETTLING_BOUND_NS,
        "walls disagreed by {} µs while the master re-acquired",
        r.max_settling_disagreement_ns / US
    );
    assert!(
        r.max_relative_phase_ns <= 50 * US,
        "relative phase {} µs",
        r.max_relative_phase_ns / US
    );
}

#[test]
fn the_1_14_restart_path_steps_every_follower_at_its_own_instant_126() {
    // The negative control: the pre-1.15 master and followers. The bench reproduces the incident.
    let sc = restart_scenario("restart: 1.14 (boot step, 30 s fallback)", false, 0);
    let r = run(&sc);
    let rl = &r.restart;
    let restart_ns = at_s(RESTART_AT) * 1e9;
    println!(
        "[{}] {rl:?}; local steps {:?}; max wall disagreement {} ms",
        sc.label,
        r.local_steps,
        r.max_disagreement_ns / MS
    );
    assert!(!rl.restored);
    assert_eq!(rl.seq_before, Some(2));
    assert_eq!(rl.seq_after, Some(1), "a new session");
    let boot = rl.boot_step_ns.expect("the boot step to UTC");
    let since_last_step = at_s(RESTART_AT + RESTART_GAP - FIRST_REQUEST_AT) - 10.0;
    assert!(
        (boot - accrued_ns(17.6, since_last_step)).abs() < 5 * MS,
        "an hour at +17.6 ppm since the last step: {boot}"
    );
    // Every follower moved its wall by the boot step — on its own NTP path or at the re-join —
    // at its own instant.
    let mut instants_s = Vec::new();
    for i in 1..6 {
        let local: Vec<f64> = r.local_steps[i]
            .iter()
            .filter(|s| (s.1 - boot).abs() < 5 * MS)
            .map(|s| at_s(s.0))
            .collect();
        let joins: Vec<f64> = r.steps[i]
            .iter()
            .filter(|s| s.2 == StepKind::Join && s.3 > restart_ns && (s.1 - boot).abs() < 5 * MS)
            .map(|s| s.3 / 1e9)
            .collect();
        assert_eq!(
            local.len() + joins.len(),
            1,
            "box {i}: local {local:?}, joins {joins:?}"
        );
        instants_s.extend(local.into_iter().chain(joins));
    }
    let local_count = r
        .local_steps
        .iter()
        .skip(1)
        .filter(|l| !l.is_empty())
        .count();
    assert!(
        local_count >= 3,
        "most followers stepped on their own NTP path: {local_count}"
    );
    let spread = instants_s.iter().cloned().fold(f64::MIN, f64::max)
        - instants_s.iter().cloned().fold(f64::MAX, f64::min);
    assert!(spread > 1.0, "uncoordinated: {instants_s:?}");
    assert!(
        r.max_disagreement_ns > 30 * MS,
        "the fleet split for seconds: {} ms",
        r.max_disagreement_ns / MS
    );
}

/// The UTC-vs-fleet error accrued over `seconds` at `ppm`, ns.
fn accrued_ns(ppm: f64, seconds: f64) -> i64 {
    (ppm * seconds * 1_000.0).round() as i64
}
