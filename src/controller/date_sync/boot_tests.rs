//! dantesync#134 — fast boot sync: the fleet date is placed at the first PTP windows, not at the
//! rate servo's lock (~31-40 s later), with ONE step, on the master and on a follower; the PTP
//! loop never waits for the boot NTP reading; and the later engagement of the phase lock never
//! takes a second step. The pure core is proven in `crate::ptp_phase_lock` (the early anchor and
//! the boot hold), the acquisition on two clocks in `tests/two_clock_bench/boot.rs`.
//!
//! Review of PR 135: the boot step resets the PTP measurement even before the first window; a late
//! reading on the fleet line is never published; a failed / timed-out / dead boot reading never
//! leaves a blocking NTP measurement to the loop (a reading is reused, else re-armed on a thread);
//! the saved state is judged provisionally until the lock (an interim grandmaster); the master's
//! NTP server says "unsynchronized" while its wall is not placed; the acquiring master's UTC error
//! carries its phase-lock error.

use super::restart_file;
use super::restart_tests::{ntp_at, restart_config, status, Ctl};
use super::tests::{authority_reply, one_offset, with_authority, PL_GM, PL_PTP_NOW_NS};
use super::*;
use crate::clock::MockSystemClock;
use crate::date_offset::{AuthorityState, DateOffsetState};
use crate::ptp_phase_lock::ANCHOR_CONFIRM_WINDOWS;
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
/// strih-pp, 25.10.2026: its RTC came back 3.75 s behind the fleet line.
const RTC_BEHIND_MS: i64 = 3_750;
/// Another grandmaster's uptime: three days off the saved line (an interim leader at power-up).
const INTERIM_GM_NS: i64 = 3 * 86_400 * S;

/// A clock that steps exactly once, by `delta_ns`.
fn steps_once(delta_ns: i64) -> MockSystemClock {
    let mut clock = MockSystemClock::new();
    let want = Duration::from_nanos(delta_ns.unsigned_abs());
    let sign: i8 = if delta_ns > 0 { 1 } else { -1 };
    clock
        .expect_step_clock()
        .times(1)
        .withf(move |dur, sg| *dur == want && *sg == sign)
        .returning(|_, _| Ok(()));
    clock
}

/// A node as `main` starts it since 1.19: the saved state read (a master), the boot NTP reading
/// handed to a channel (nothing measured inline: the NTP source must not be called before the
/// reading arrives — `MockNtpSource::new()` panics on any call), NTP server mode on a master.
fn booted(
    clock: MockSystemClock,
    master: bool,
    path: Option<&Path>,
) -> (Ctl, Sender<BootNtpResult>) {
    booted_with(clock, MockNtpSource::new(), master, path, restart_config())
}

/// [`booted`] with the NTP source the loop's periodic checks read once the boot reading is in,
/// and a config.
fn booted_with(
    mut clock: MockSystemClock,
    ntp: MockNtpSource,
    master: bool,
    path: Option<&Path>,
    config: SystemConfig,
) -> (Ctl, Sender<BootNtpResult>) {
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        ntp,
        Arc::new(RwLock::new(SyncStatus::default())),
        config,
    );
    if let Some(p) = path {
        c.open_date_state(p, master);
    }
    let (tx, rx) = channel();
    c.start_boot_ntp(rx);
    if master {
        c.configure_ntp_server_mode(100_000);
    }
    (c, tx)
}

/// One PTP window whose median `t2 − t1` is `median`, then one loop iteration. `locked` is the
/// rate servo's verdict (before it, the box is acquiring). Outside the 2 s grace of a step.
fn window(c: &mut Ctl, median: i64, locked: bool) {
    c.current_gm_uuid = Some(PL_GM);
    c.last_ntp_step = None;
    c.is_locked = locked;
    c.lock_stable_count = if locked { LOCK_STABLE_COUNT } else { 0 };
    c.date_sync.pending_median_ns = Some(median);
    c.date_sync.pending_t1_ns = wall_now_ns() - median;
    c.apply_self_tuning_servo(0.0);
    assert_eq!(c.is_locked, locked, "the lock verdict held for this window");
    c.service_boot_ntp();
    c.service_date_offset();
}

/// The first unlocked windows that anchor `D` (three agreeing ones).
fn first_windows(c: &mut Ctl, median: i64) {
    for _ in 0..ANCHOR_CONFIRM_WINDOWS {
        window(c, median, false);
    }
    assert!(c.date_sync.core.anchor_ns().is_some(), "anchored");
}

/// `n` raw PTP samples with `t2 − t1 = diff`, through the REAL sample path
/// (`process_settled_sync`: the sample window, the raw window, the 4-sample close), each followed
/// by one loop iteration.
fn samples(c: &mut Ctl, diff: i64, n: usize) {
    for _ in 0..n {
        let t2 = wall_now_ns();
        c.process_settled_sync(t2 - diff, t2, 0);
        c.service_boot_ntp();
        c.service_date_offset();
    }
}

fn saved_at(d: i64, seq: u32, age: i64) -> DateOffsetState {
    let now = wall_now_ns();
    DateOffsetState {
        authority: AuthorityState {
            d_ns: d,
            since_ptp_ns: now - d - age - 5 * S,
            seq,
            pending: None,
            slew: None,
            micro: false,
            daily_last_step: None,
        },
        gm_uuid: PL_GM,
        written_wall_ns: now - age,
        written_ptp_ns: now - d - age,
    }
}

fn saved(d: i64, seq: u32) -> DateOffsetState {
    saved_at(d, seq, 60 * S)
}

/// A saved state for `fleet` (seq 2) in a fresh directory.
fn with_saved(state: DateOffsetState) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    restart_file::write_atomic(&path, &state).expect("written");
    (dir, path)
}

/// A master with a saved state placed on the fleet line at its first windows: restored, one step.
/// `ntp` serves the loop's periodic checks after the boot reading.
fn master_placed_at_the_first_windows(
    fleet: i64,
    ntp: MockNtpSource,
) -> (Ctl, Sender<BootNtpResult>, tempfile::TempDir) {
    let (dir, path) = with_saved(saved(fleet, 2));
    let (mut c, tx) = booted_with(
        steps_once(RTC_BEHIND_MS * MS),
        ntp,
        true,
        Some(&path),
        restart_config(),
    );
    assert!(c.boot_step_deferred(), "the saved state waits for PTP");
    assert!(
        status(&c).ntp_server_unsynchronized,
        "its wall is not placed yet: the NTP server must not serve it as synchronized"
    );
    // The first windows: not locked (the rate servo has just started), the wall 3.75 s behind.
    first_windows(&mut c, fleet - RTC_BEHIND_MS * MS);
    assert!(!c.date_sync.core.engaged(), "still acquiring");
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(st.date_offset_restored, "restored at the first windows");
    assert_eq!(st.date_offset_ns, Some(fleet));
    assert_eq!(st.date_offset_seq, Some(2), "the same session");
    assert_eq!(st.date_step_pending_ns, None, "this master is on the line");
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    assert_eq!(st.last_date_step_kind, "join");
    assert!(
        !st.ntp_server_unsynchronized,
        "placed on the fleet line: synchronized (its boot reading is not needed)"
    );
    (c, tx, dir)
}

#[test]
fn a_master_with_a_saved_state_places_the_date_at_the_first_windows_with_one_step_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (mut c, tx, _dir) = master_placed_at_the_first_windows(fleet, MockNtpSource::new());
    // The acquisition drift (the rate servo has not removed the oscillator error yet): no step.
    window(&mut c, fleet + 300 * 1_000, false);
    window(&mut c, fleet + 900 * 1_000, false);
    // The lock, 1.4 ms off the line: beyond the 1 ms re-anchor bound, held — no second step.
    window(&mut c, fleet + 1_400 * 1_000, true);
    assert!(c.date_sync.core.engaged());
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet), "no re-anchor");
    assert!(c.date_sync.core.pull_us() > 1_000.0, "the drift is pulled");
    // The boot reading arrives late (12 s to an internet upstream): report-only, the master is on
    // the fleet line already. (The clock allows ONE step: a second would panic at drop.)
    tx.send(Ok(one_offset(RTC_BEHIND_MS * 1_000 + 120, 1)))
        .expect("send");
    c.service_boot_ntp();
    window(&mut c, fleet + 1_300 * 1_000, true);
    let st = status(&c);
    assert_eq!(st.date_offset_ns, Some(fleet));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
}

#[test]
fn a_late_boot_reading_on_the_fleet_line_is_logged_never_published_134() {
    // It was measured before the wall step the restore already applied: published, it would show
    // +3.75 s as this master's NTP offset and serve a reference epoch 3.75 s in the future.
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (mut c, tx, _dir) = master_placed_at_the_first_windows(fleet, MockNtpSource::new());
    let before = status(&c);
    tx.send(Ok(one_offset(RTC_BEHIND_MS * 1_000 + 120, 1)))
        .expect("send");
    c.service_boot_ntp();
    let after = status(&c);
    assert_eq!(after.ntp_offset_us, before.ntp_offset_us);
    assert_eq!(after.ntp_updated_ts, before.ntp_updated_ts);
    assert!(
        c.last_ntp_success.is_none(),
        "no NTP success recorded from it"
    );
}

#[test]
fn a_master_without_a_saved_state_takes_its_boot_step_from_the_late_reading_then_its_authority_134()
{
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    assert!(status(&c).ntp_server_unsynchronized, "the boot step is due");
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    first_windows(&mut c, d0);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d0), "anchored early");
    assert!(
        c.date_sync.authority.is_none(),
        "no session on a wall the boot step has not placed yet"
    );
    assert!(status(&c).ntp_server_unsynchronized);
    // The loop's periodic NTP check would block the loop on the same unanswered upstream: it
    // waits for the boot reading (the NTP mock panics if called).
    c.last_ntp_check = Instant::now() - Duration::from_secs(120);
    c.check_ntp_utc_tracking();
    // The reading arrives: the boot step (+250 ms), D moves with the wall …
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d0 + 250 * MS));
    assert!(!status(&c).ntp_server_unsynchronized, "the wall is placed");
    // … and the new session starts on the stepped wall.
    c.service_date_offset();
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(!st.date_offset_restored);
    assert_eq!(st.date_offset_ns, Some(d0 + 250 * MS));
    assert_eq!(st.date_offset_seq, Some(1));
    // The lock 0.6 ms later: no second step.
    window(&mut c, d0 + 250 * MS + 600 * 1_000, false);
    window(&mut c, d0 + 250 * MS + 600 * 1_000, true);
    assert!(c.date_sync.core.engaged());
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d0 + 250 * MS));
}

#[test]
fn a_boot_step_before_the_first_window_drops_the_samples_taken_before_it_134() {
    // Review of PR 135, finding 1: through the REAL sample path. Three samples are taken on the
    // un-stepped wall, then the reading steps it by +250 ms before the window closes. Without the
    // reset the window mixes them with post-step samples and anchors D on the pre-step wall.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    samples(&mut c, d0, 3);
    assert_eq!(c.date_sync.window.len(), 3);
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    assert!(
        c.date_sync.window.is_empty() && c.sample_window.is_empty(),
        "the pre-step samples are dropped"
    );
    assert!(c.last_ntp_step.is_some(), "the post-step grace runs");
    assert!(c.last_offset_us.is_none(), "no rate kick from the step");
    // Past the grace, the post-step samples anchor D on the stepped wall.
    c.last_ntp_step = Some(Instant::now() - Duration::from_secs(3));
    samples(&mut c, d0 + 250 * MS, 4 * ANCHOR_CONFIRM_WINDOWS);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d0 + 250 * MS));
}

#[test]
fn a_legacy_node_resets_its_rate_servo_after_a_late_boot_step_134() {
    // The legacy discipline has no phase lock: a step inside a sample window would read as a
    // ±500 ppm rate.
    let mut config = restart_config();
    config.clock_discipline = crate::config::CLOCK_DISCIPLINE_LEGACY.to_string();
    let (mut c, tx) = booted_with(
        steps_once(250 * MS),
        MockNtpSource::new(),
        false,
        None,
        config,
    );
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    samples(&mut c, d0, 6);
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    assert!(
        c.sample_window.is_empty(),
        "the pre-step samples are dropped"
    );
    assert!(
        c.last_offset_us.is_none(),
        "the rate servo restarts its rate"
    );
    assert!(c.last_ntp_step.is_some(), "the post-step grace runs");
}

#[test]
fn a_follower_joins_the_fleet_line_at_its_first_windows_and_never_steps_again_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (mut c, tx) = booted(steps_once(40 * MS), false, None);
    let slot = with_authority(&mut c);
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, fleet, PL_PTP_NOW_NS - 600 * S, 2));
    assert!(
        !status(&c).ntp_server_unsynchronized,
        "a follower serves no NTP"
    );
    // The first windows, 40 ms behind the fleet line, not locked: ONE join step.
    first_windows(&mut c, fleet - 40 * MS);
    assert_eq!(c.date_sync.follower.adopted_seq(), Some(2));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    assert_eq!(status(&c).date_authority, "follower");
    // Its boot reading (the master's NTP server) comes after it: report-only.
    tx.send(Ok(one_offset(900, 1))).expect("send");
    c.service_boot_ntp();
    // Acquisition drift, then the lock 1.3 ms off: held, no second step.
    window(&mut c, fleet + 500 * 1_000, false);
    window(&mut c, fleet + 1_300 * 1_000, true);
    assert!(c.date_sync.core.engaged());
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    assert_eq!(c.date_sync.follower.late_steps(), 0);
}

#[test]
fn a_grandmaster_lost_before_the_lock_keeps_the_restored_date_and_takes_no_step_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    // After the boot reading the master's periodic NTP checks run (daily mode, its own PTP off:
    // report-only, they feed the authority).
    let (mut c, tx, _dir) = master_placed_at_the_first_windows(fleet, ntp_at(120));
    tx.send(Ok(one_offset(120, 1))).expect("send");
    c.service_boot_ntp();
    // PTP is gone before the rate servo locked.
    c.ptp_offline = true;
    c.on_ptp_offline_edge();
    for _ in 0..3 {
        c.service_date_offset();
        c.last_ntp_check = Instant::now() - Duration::from_secs(120);
        c.check_ntp_utc_tracking();
    }
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert_eq!(st.date_offset_ns, Some(fleet), "the fleet date stays");
    assert_eq!(st.date_offset_seq, Some(2));
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    // PTP back, the same time base: the free-run (60 µs) is absorbed, nothing stepped.
    c.ptp_offline = false;
    window(&mut c, fleet + 60 * 1_000, false);
    window(&mut c, fleet + 70 * 1_000, true);
    assert!(c.date_sync.core.engaged());
    assert_eq!(status(&c).date_offset_ns, Some(fleet));
}

#[test]
fn a_boot_reading_that_times_out_never_leaves_a_blocking_measurement_to_the_loop_134() {
    // Review finding 5: a rejected saved state (a stale record) wants the boot step, the reading
    // never comes. After the wait the master starts on its current wall (the pre-1.19 "NTP Sync
    // failed" path) — it must not measure again inside the loop (the mock panics if it does).
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved_at(fleet, 2, 2 * 86_400 * S));
    let (mut c, _tx) = booted(MockSystemClock::new(), true, Some(&path));
    first_windows(&mut c, fleet);
    assert!(
        c.date_sync.authority.is_none(),
        "waiting for the boot reading"
    );
    c.date_sync
        .restart
        .boot_ntp
        .as_mut()
        .expect("pending")
        .since = Instant::now() - super::boot::BOOT_NTP_WAIT - Duration::from_secs(1);
    c.service_boot_ntp();
    c.service_date_offset();
    assert!(!c.date_sync.restart.boot_step_due);
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert_eq!(st.date_offset_ns, Some(fleet), "the current wall");
    assert_eq!(st.date_offset_seq, Some(1));
}

#[test]
fn a_dead_boot_thread_is_treated_as_a_failed_reading_134() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, tx) = booted(MockSystemClock::new(), true, Some(&path));
    drop(tx);
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    first_windows(&mut c, d0);
    assert!(!c.boot_ntp_pending());
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert_eq!(st.date_offset_ns, Some(d0));
    assert!(!st.ntp_server_unsynchronized);
}

#[test]
fn a_rejected_restore_reuses_the_boot_reading_taken_before_the_first_window_134() {
    // Review finding 5: the reading came first (skipped: a saved state waited), then the restore is
    // rejected (a stale record): the deferred boot step uses that reading — no second, blocking
    // measurement in the loop.
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved_at(fleet, 2, 2 * 86_400 * S));
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    assert!(c.boot_step_deferred(), "skipped for the saved state");
    first_windows(&mut c, fleet);
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(!st.date_offset_restored);
    assert_eq!(st.date_offset_seq, Some(1), "a new session");
    assert_eq!(
        st.date_offset_ns,
        Some(fleet + 250 * MS),
        "on the stepped wall"
    );
}

#[test]
fn a_saved_state_given_up_without_a_reading_measures_again_on_a_thread_134() {
    // No PTP window within the wait and no boot reading left (the boot thread died): the boot step
    // is measured again on a thread (the spawner `main` provides), never inside the loop.
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved(fleet, 2));
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    let spawned: Arc<std::sync::Mutex<Vec<Sender<BootNtpResult>>>> = Arc::default();
    let s2 = spawned.clone();
    c.set_boot_ntp_spawner(Box::new(move || {
        let (t, r) = channel();
        s2.lock().unwrap().push(t);
        r
    }));
    drop(tx);
    c.service_boot_ntp();
    c.date_sync.restart.loaded_at =
        Instant::now() - restart::RESTORE_WAIT_FOR_PTP - Duration::from_secs(1);
    c.service_date_offset();
    assert_eq!(spawned.lock().unwrap().len(), 1, "one new boot reading");
    assert!(c.boot_ntp_pending());
    spawned.lock().unwrap()[0]
        .send(Ok(one_offset(250_000, 1)))
        .expect("send");
    c.service_boot_ntp();
    assert!(!c.date_sync.restart.boot_step_due, "the boot step ran");
    assert!(!c.boot_step_deferred());
}

#[test]
fn a_restore_judged_on_an_interim_grandmaster_waits_for_the_real_one_134() {
    // Review finding 4: at a cold power-up the first leader may be an interim one (another uptime).
    // Rejecting the saved state for good there would start seq 1 = move the fleet date. Before the
    // lock a time-base rejection is provisional: the master waits (no session, no step) and judges
    // again on the next time base.
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved(fleet, 2));
    let (mut c, _tx) = booted(steps_once(RTC_BEHIND_MS * MS), true, Some(&path));
    first_windows(&mut c, fleet + INTERIM_GM_NS);
    assert!(c.date_sync.authority.is_none(), "no new session");
    assert!(c.boot_step_deferred(), "the saved state still waits");
    assert!(!c.date_sync.restart.boot_step_due);
    // The real grandmaster takes over (its time base is the saved one).
    c.date_sync.on_time_base_change();
    window(&mut c, fleet - RTC_BEHIND_MS * MS, false);
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(st.date_offset_restored);
    assert_eq!(st.date_offset_seq, Some(2), "the same session");
    assert_eq!(st.date_offset_ns, Some(fleet));
}

#[test]
fn a_restore_still_off_at_the_lock_is_rejected_then_and_the_boot_step_runs_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved(fleet, 2));
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    first_windows(&mut c, fleet + INTERIM_GM_NS);
    assert!(c.date_sync.authority.is_none());
    // The lock on the same (other) time base: the rejection is final now.
    window(&mut c, fleet + INTERIM_GM_NS, true);
    c.service_date_offset();
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(!st.date_offset_restored);
    assert_eq!(st.date_offset_seq, Some(1));
    assert_eq!(st.date_offset_ns, Some(fleet + INTERIM_GM_NS + 250 * MS));
}

#[test]
fn an_acquiring_master_feeds_the_fleet_line_its_phase_lock_error_134() {
    // Review finding 6: while acquiring, its wall is up to the 2 ms hold off the line it serves.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, tx) = booted_with(
        MockSystemClock::new(),
        ntp_at(0),
        true,
        Some(&path),
        restart_config(),
    );
    tx.send(Ok(one_offset(0, 1))).expect("send");
    c.service_boot_ntp();
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    first_windows(&mut c, d0);
    assert!(c.date_sync.authority.is_some());
    window(&mut c, d0 + 1_500 * 1_000, false);
    c.last_ntp_check = Instant::now() - Duration::from_secs(120);
    c.check_ntp_utc_tracking();
    // The wall is 1.5 ms ahead of the line and reads UTC exactly: the line is 1.5 ms behind UTC.
    assert_eq!(c.date_sync.master_utc_error_ns, Some(1_500 * 1_000));
}

#[test]
fn an_accepted_restore_whose_join_step_fails_stays_unsynchronized_until_it_lands_134() {
    // Re-review finding 1: the restore is accepted, but the master's own join step onto the fleet
    // line fails (it retries after the 10 s backoff). Meanwhile its wall is seconds off the line:
    // its NTP server must keep answering "unsynchronized".
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved(fleet, 2));
    let mut clock = MockSystemClock::new();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c2 = calls.clone();
    clock.expect_step_clock().times(2).returning(move |_, _| {
        if c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Err(anyhow::anyhow!("EPERM"))
        } else {
            Ok(())
        }
    });
    let (mut c, tx) = booted(clock, true, Some(&path));
    // The boot reading is in (skipped for the saved state): nothing else is pending.
    tx.send(Ok(one_offset(RTC_BEHIND_MS * 1_000, 1)))
        .expect("send");
    c.service_boot_ntp();
    first_windows(&mut c, fleet - RTC_BEHIND_MS * MS);
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(st.date_offset_restored);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the join failed"
    );
    assert!(
        st.ntp_server_unsynchronized,
        "its wall is 3.75 s off the line it publishes"
    );
    // The backoff ends, the join lands: synchronized.
    c.date_sync.step_failed_at = Some(Instant::now() - Duration::from_secs(60));
    window(&mut c, fleet - RTC_BEHIND_MS * MS, false);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    assert!(!status(&c).ntp_server_unsynchronized);
    // Once placed, a later PTP outage keeps serving (the fallback holds after the first placement).
    c.ptp_offline = true;
    c.on_ptp_offline_edge();
    c.service_date_offset();
    assert!(!status(&c).ntp_server_unsynchronized);
}

/// A boot reading refused because the master's NTP server says it is unsynchronized.
fn unsynchronized() -> BootNtpResult {
    Err(BootNtpError {
        message: "Server reply indicates that the server clock is not synchronized".to_string(),
        server_unsynchronized: true,
    })
}

#[test]
fn a_follower_asks_an_unsynchronized_master_again_a_few_seconds_later_134() {
    // Re-review finding 3: the master answers LI = 3 while its own wall is not placed. A follower
    // must not give its boot reading up and start on its raw wall: it asks again shortly.
    let (mut c, tx) = booted(steps_once(250 * MS), false, None);
    let spawned: Arc<std::sync::Mutex<Vec<Sender<BootNtpResult>>>> = Arc::default();
    let s2 = spawned.clone();
    c.set_boot_ntp_spawner(Box::new(move || {
        let (t, r) = channel();
        s2.lock().unwrap().push(t);
        r
    }));
    tx.send(unsynchronized()).expect("send");
    c.service_boot_ntp();
    assert!(c.boot_ntp_pending(), "a retry is scheduled");
    assert!(spawned.lock().unwrap().is_empty(), "not at once");
    c.date_sync.restart.boot_retry_at = Some(Instant::now() - Duration::from_millis(1));
    c.service_boot_ntp();
    assert_eq!(spawned.lock().unwrap().len(), 1, "asked again");
    spawned.lock().unwrap()[0]
        .send(Ok(one_offset(250_000, 1)))
        .expect("send");
    c.service_boot_ntp();
    assert!(!c.boot_ntp_pending(), "the boot step ran");
}

#[test]
fn the_retries_of_an_unsynchronized_master_are_bounded_and_other_failures_not_retried_134() {
    let (mut c, tx) = booted(MockSystemClock::new(), false, None);
    let spawned: Arc<std::sync::Mutex<Vec<Sender<BootNtpResult>>>> = Arc::default();
    let s2 = spawned.clone();
    c.set_boot_ntp_spawner(Box::new(move || {
        let (t, r) = channel();
        s2.lock().unwrap().push(t);
        r
    }));
    tx.send(unsynchronized()).expect("send");
    c.service_boot_ntp();
    for n in 0..super::boot::BOOT_UNSYNC_RETRIES {
        assert!(c.boot_ntp_pending(), "retry {n} scheduled");
        c.date_sync.restart.boot_retry_at = Some(Instant::now() - Duration::from_millis(1));
        c.service_boot_ntp();
        spawned.lock().unwrap()[n]
            .send(unsynchronized())
            .expect("send");
        c.service_boot_ntp();
    }
    assert!(
        !c.boot_ntp_pending(),
        "the budget is spent: on its current wall"
    );
    assert_eq!(
        spawned.lock().unwrap().len(),
        super::boot::BOOT_UNSYNC_RETRIES
    );
    // Any other failure is not retried.
    let (mut c, tx) = booted(MockSystemClock::new(), false, None);
    c.set_boot_ntp_spawner(Box::new(|| channel().1));
    tx.send(Err(BootNtpError::other("timeout"))).expect("send");
    c.service_boot_ntp();
    assert!(!c.boot_ntp_pending());
}

#[test]
fn the_end_of_the_wait_names_a_provisional_rejection_134() {
    // Re-review finding 4.
    let plain = restart::restore_wait_expiry_message(None);
    assert!(plain.contains("no PTP window"), "{plain}");
    let named = restart::restore_wait_expiry_message(Some("another time base"));
    assert!(
        named.contains("provisionally") && named.contains("another time base"),
        "{named}"
    );
    assert!(!named.contains("no PTP window"), "{named}");
    // The provisional rejection is kept for it.
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (_dir, path) = with_saved(saved(fleet, 2));
    let (mut c, _tx) = booted(MockSystemClock::new(), true, Some(&path));
    first_windows(&mut c, fleet + INTERIM_GM_NS);
    assert!(c.date_sync.restart.provisional_rejection.is_some());
}
