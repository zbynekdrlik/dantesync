//! dantesync#126 — the controller wiring of a master restart that keeps the fleet date: the
//! saved state skips the boot step and keeps the master off its NTP step path until the first
//! PTP lock, is restored there (same D, same seq) or rejected (the boot step then runs from the
//! loop, a new session), is saved whenever it changes; and a follower holds the fleet date through
//! the restart and re-joins the restored master with no step. The pure record and decision are
//! proven in `crate::date_offset` (persist / authority_restore), end-to-end in the two-clock bench.

use super::restart_file;
use super::tests::{
    anchored_controller_with, authority_reply, one_offset, phase_lock_config, with_authority,
    PL_GM, PL_PTP_NOW_NS,
};
use super::*;
use crate::clock::MockSystemClock;
use crate::date_offset::{AuthorityState, DateOffsetState};
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::path::Path;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
const OTHER_GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0d];
/// The day's drift the restarted master reads at boot (30.9.2026: +247.297 ms).
const BOOT_ERROR_US: i64 = 247_297;

pub(super) type Ctl = PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>;

/// The fleet default (the nightly step) with its window 12 h from now: nothing nightly happens
/// inside a test.
pub(super) fn restart_config() -> SystemConfig {
    let mut config = phase_lock_config();
    config.date_offset.correction = "daily".to_string();
    let tod = (wall_now_ns() / S + 12 * 3_600).rem_euclid(86_400);
    config.date_offset.daily_step_utc =
        format!("{:02}:{:02}:{:02}", tod / 3_600, tod % 3_600 / 60, tod % 60);
    config
}

pub(super) fn ntp_at(us: i64) -> MockNtpSource {
    let mut ntp = MockNtpSource::new();
    ntp.expect_get_offset()
        .returning(move || Ok(one_offset(us.abs(), if us < 0 { -1 } else { 1 })));
    ntp
}

pub(super) fn status(c: &Ctl) -> SyncStatus {
    c.update_shared_status();
    let st = c.get_status_shared();
    let st = st.read().expect("status");
    st.clone()
}

/// A controller as `main` builds a MASTER: the saved state read (when `path`), then the boot
/// `run_ntp_sync`, then NTP server mode.
fn started(mut clock: MockSystemClock, ntp: MockNtpSource, path: &Path) -> Ctl {
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        ntp,
        Arc::new(RwLock::new(SyncStatus::default())),
        restart_config(),
    );
    c.load_date_state(path.to_path_buf());
    c.run_ntp_sync(false);
    c.configure_ntp_server_mode(100_000);
    c
}

/// The first PTP lock: the window's median `t2 − t1` is `anchor`, under grandmaster `gm`.
fn first_lock(c: &mut Ctl, gm: [u8; 6], anchor: i64) {
    c.current_gm_uuid = Some(gm);
    c.is_locked = true;
    c.date_sync.pending_median_ns = Some(anchor);
    c.date_sync.pending_t1_ns = wall_now_ns() - anchor;
    c.apply_self_tuning_servo(0.0);
    assert_eq!(c.date_sync.core.anchor_ns(), Some(anchor), "the first lock");
}

/// Three NTP cycles (the master's cadence is 10 s; aged by hand).
fn ntp_cycles(c: &mut Ctl) {
    for _ in 0..3 {
        c.last_ntp_check = Instant::now() - Duration::from_secs(120);
        c.check_ntp_utc_tracking();
    }
}

/// A saved state for the fleet line `d` under `gm`, as seq `seq`, written a minute ago.
fn saved_state(gm: [u8; 6], d: i64, seq: u32) -> DateOffsetState {
    let now = wall_now_ns();
    DateOffsetState {
        authority: AuthorityState {
            d_ns: d,
            since_ptp_ns: now - d - 5 * S,
            seq,
            pending: None,
            slew: None,
            micro: false,
            daily_last_step: None,
        },
        gm_uuid: gm,
        written_wall_ns: now - 60 * S,
        written_ptp_ns: now - d - 60 * S,
    }
}

#[test]
fn a_restarted_master_keeps_the_fleet_date_and_its_follower_never_steps_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");

    // The first master: no saved state yet (the boot reading is small), anchored on the fleet
    // line, a grandmaster event earlier in its life (seq 2, the same D). It saves its state.
    let mut first = MockSystemClock::new();
    first.expect_step_clock().times(0);
    let mut m1 = started(first, ntp_at(20), &path);
    assert!(!m1.boot_step_deferred(), "nothing saved yet");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    first_lock(&mut m1, PL_GM, d);
    let now_ptp = wall_now_ns() - d;
    m1.date_sync
        .authority
        .as_mut()
        .expect("the master's authority")
        .rebase(d, now_ptp);
    m1.service_date_offset();
    let st1 = status(&m1);
    assert_eq!(st1.date_authority, "master");
    assert_eq!(st1.date_offset_seq, Some(2));
    assert!(!st1.date_offset_restored, "a new session");
    let saved = restart_file::read(&path)
        .expect("readable")
        .expect("the master saved its state");
    assert_eq!(
        (saved.authority.d_ns, saved.authority.seq, saved.gm_uuid),
        (d, 2, PL_GM)
    );

    // A follower adopted it.
    let mut fclock = MockSystemClock::new();
    fclock.expect_step_clock().times(0);
    let (mut f, _) = anchored_controller_with(fclock, ntp_at(5_000), false, restart_config());
    // On the same fleet line (built a moment later, its own anchor would differ by the real time
    // between the two).
    f.date_sync.core.set_anchor(d);
    let slot = with_authority(&mut f);
    let eff = st1.date_offset_effective_ptp_ns.expect("published");
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, eff, 2));
    f.service_date_offset();
    assert_eq!(f.date_sync.follower.adopted_seq(), Some(2));

    // The master stops. Past the 30 s loss the follower HOLDS the fleet date; its NTP readings
    // (the master's NTP server) are report-only.
    drop(m1);
    f.date_sync.last_applicable_reply =
        Some(Instant::now() - AUTHORITY_LOSS - Duration::from_secs(60));
    f.service_date_offset();
    let sf = status(&f);
    assert_eq!(sf.date_authority, "holding");
    assert!(sf.date_authority_hold_age_s.is_some());
    ntp_cycles(&mut f);

    // The master restarts. Its boot reading is the day's drift: NO boot step, and no NTP step
    // before its first lock either.
    let mut second = MockSystemClock::new();
    second.expect_step_clock().times(0);
    let mut m2 = started(second, ntp_at(BOOT_ERROR_US), &path);
    assert!(
        m2.boot_step_deferred(),
        "the saved state waits for the first lock"
    );
    ntp_cycles(&mut m2);
    // Its wall is where the fleet line is (a service restart: the kernel kept the clock).
    first_lock(&mut m2, PL_GM, d);
    m2.service_date_offset();
    let st2 = status(&m2);
    assert_eq!(st2.date_authority, "master");
    assert!(st2.date_offset_restored, "restored, not re-derived");
    assert_eq!(st2.date_offset_ns, Some(d), "the same fleet D");
    assert_eq!(st2.date_offset_seq, Some(2), "the same session");
    assert_eq!(
        st2.date_step_pending_ns, None,
        "on the line: nothing to re-join"
    );
    // By day its UTC readings only feed the authority (the drift waits for the night).
    ntp_cycles(&mut m2);

    // The follower hears the restored master: no step, following again.
    *slot.lock().unwrap() = Some(authority_reply(
        2,
        PL_GM,
        st2.date_offset_ns.expect("D"),
        st2.date_offset_effective_ptp_ns.expect("instant"),
        st2.date_offset_seq.expect("seq"),
    ));
    f.service_date_offset();
    let sf = status(&f);
    assert_eq!(sf.date_authority, "follower");
    assert_eq!(sf.date_authority_hold_age_s, None);
    assert_eq!(f.date_sync.follower.adopted_seq(), Some(2));
    assert_eq!(f.date_sync.follower.late_steps(), 0);
}

#[test]
fn the_boot_step_is_skipped_only_while_a_readable_saved_state_waits_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    let good = dir.path().join("good.json");
    restart_file::write_atomic(&good, &saved_state(PL_GM, d, 4)).expect("written");
    let torn = dir.path().join("torn.json");
    std::fs::write(&torn, "{torn").expect("written");
    let newer = dir.path().join("newer.json");
    let v2 =
        restart_file::encode(&saved_state(PL_GM, d, 4)).replace("\"version\": 1", "\"version\": 2");
    std::fs::write(&newer, v2).expect("written");
    let missing = dir.path().join("missing.json");
    for (path, boot_steps) in [(&good, 0_usize), (&missing, 1), (&torn, 1), (&newer, 1)] {
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(boot_steps)
            .withf(|dur, sign| *dur == Duration::from_micros(BOOT_ERROR_US as u64) && *sign == 1)
            .returning(|_, _| Ok(()));
        let c = started(clock, ntp_at(BOOT_ERROR_US), path);
        assert_eq!(
            c.boot_step_deferred(),
            boot_steps == 0,
            "{}",
            path.display()
        );
    }
}

#[test]
fn a_saved_state_under_another_grandmaster_runs_the_boot_step_at_the_first_lock_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(OTHER_GM, d, 4)).expect("written");
    let mut clock = MockSystemClock::new();
    // Only the deferred boot step, taken from the loop after the rejection.
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sign| *dur == Duration::from_micros(BOOT_ERROR_US as u64) && *sign == 1)
        .returning(|_, _| Ok(()));
    let mut c = started(clock, ntp_at(BOOT_ERROR_US), &path);
    assert!(c.boot_step_deferred());
    first_lock(&mut c, PL_GM, d);
    assert!(
        c.date_sync.authority.is_none(),
        "no session before the boot step"
    );
    assert!(c.date_sync.restart.boot_step_due);
    c.service_date_offset();
    assert!(!c.date_sync.restart.boot_step_due, "it ran");
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d + BOOT_ERROR_US * 1_000),
        "D moved with the wall"
    );
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert_eq!(
        st.date_offset_seq,
        Some(1),
        "a new session (the pre-1.15 path)"
    );
    assert!(!st.date_offset_restored);
}

#[test]
fn a_master_waiting_for_its_first_lock_takes_no_ntp_step_and_gives_up_after_the_wait_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(PL_GM, d, 4)).expect("written");
    let mut clock = MockSystemClock::new();
    // One step only: the boot step once the wait for PTP is over.
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sign| *dur == Duration::from_micros(BOOT_ERROR_US as u64) && *sign == 1)
        .returning(|_, _| Ok(()));
    let mut c = started(clock, ntp_at(BOOT_ERROR_US), &path);
    ntp_cycles(&mut c);
    c.service_date_offset();
    assert!(c.boot_step_deferred(), "still waiting for PTP");
    // PTP never came.
    c.date_sync.restart.loaded_at =
        Instant::now() - restart::RESTORE_WAIT_FOR_PTP - Duration::from_secs(1);
    c.service_date_offset();
    assert!(!c.boot_step_deferred(), "the saved state is given up");
    assert!(!c.date_sync.restart.boot_step_due, "the boot step ran");
}

#[test]
fn a_restored_master_off_the_line_re_joins_its_own_wall_alone_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(PL_GM, d, 4)).expect("written");
    let mut clock = MockSystemClock::new();
    // Its wall came back 3 ms ahead of the fleet line (a host reboot's RTC): ONE step back of its
    // OWN wall; the fleet D never moves.
    clock
        .expect_step_clock()
        .times(1)
        .withf(|dur, sign| *dur == Duration::from_millis(3) && *sign == -1)
        .returning(|_, _| Ok(()));
    let mut c = started(clock, ntp_at(BOOT_ERROR_US), &path);
    first_lock(&mut c, PL_GM, d + 3 * MS);
    let st = status(&c);
    assert!(st.date_offset_restored);
    assert_eq!(st.date_offset_seq, Some(4), "the same session");
    assert_eq!(
        st.date_step_pending_ns,
        Some(-3 * MS),
        "it publishes the FLEET D: its own correction back to it"
    );
    c.service_date_offset();
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d),
        "its wall is back on the line"
    );
    let st = status(&c);
    assert_eq!(st.date_offset_ns, Some(d));
    assert_eq!(st.date_step_pending_ns, None);
    assert_eq!(st.last_date_step_kind, "join");
}

#[test]
fn the_master_saves_its_date_offset_when_it_changes_and_a_save_error_is_not_fatal_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, d) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        true,
        restart_config(),
    );
    c.date_sync.restart.path = Some(path.clone());
    c.service_date_offset();
    let saved = restart_file::read(&path)
        .expect("readable")
        .expect("written");
    assert_eq!(
        (saved.authority.d_ns, saved.authority.seq, saved.gm_uuid),
        (d, 1, PL_GM)
    );
    // Unchanged: not written again.
    std::fs::remove_file(&path).expect("removed");
    c.service_date_offset();
    assert!(!path.exists(), "written only when it changes");
    // A change is written at the next loop iteration.
    let now_ptp = wall_now_ns() - d;
    let a = c.date_sync.authority.as_mut().expect("authority");
    a.rebase(d, now_ptp);
    c.service_date_offset();
    let saved = restart_file::read(&path)
        .expect("readable")
        .expect("written");
    assert_eq!(saved.authority.seq, 2);
    // A directory that does not exist: logged, retried after a backoff, never fatal.
    c.date_sync.restart.path = Some(dir.path().join("no/such/dir/date-offset.json"));
    let a = c.date_sync.authority.as_mut().expect("authority");
    a.rebase(d, now_ptp);
    c.service_date_offset();
    assert!(c.date_sync.restart.save_failed_at.is_some());
    c.service_date_offset();
    assert_eq!(status(&c).date_offset_seq, Some(3), "the master runs on");
}

/// A master built like [`started`] but whose NTP server did not start (no server mode).
fn started_without_server(mut clock: MockSystemClock, ntp: MockNtpSource, path: &Path) -> Ctl {
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        ntp,
        Arc::new(RwLock::new(SyncStatus::default())),
        restart_config(),
    );
    c.load_date_state(path.to_path_buf());
    c.run_ntp_sync(false);
    c
}

fn boot_step_clock(times: usize, us: u64, sign: i8) -> MockSystemClock {
    let mut clock = MockSystemClock::new();
    clock
        .expect_step_clock()
        .times(times)
        .withf(move |dur, sg| *dur == Duration::from_micros(us) && *sg == sign)
        .returning(|_, _| Ok(()));
    clock
}

#[test]
fn a_master_whose_ntp_server_does_not_start_takes_the_boot_step_it_skipped_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(PL_GM, d, 4)).expect("written");
    // `main` abandons the saved state when the NTP server fails to bind.
    let clock = boot_step_clock(1, BOOT_ERROR_US as u64, 1);
    let mut c = started_without_server(clock, ntp_at(BOOT_ERROR_US), &path);
    assert!(c.boot_step_deferred());
    c.abandon_date_state();
    assert!(!c.boot_step_deferred(), "not restored");
    c.abandon_date_state(); // idempotent: no second step
                            // And the give-up after the wait runs without server mode too.
    let clock = boot_step_clock(1, BOOT_ERROR_US as u64, 1);
    let mut c = started_without_server(clock, ntp_at(BOOT_ERROR_US), &path);
    c.date_sync.restart.loaded_at =
        Instant::now() - restart::RESTORE_WAIT_FOR_PTP - Duration::from_secs(1);
    c.service_date_offset();
    assert!(!c.boot_step_deferred());
    ntp_cycles(&mut c); // no longer report-only: the ordinary path runs (nothing to step here)
}

#[test]
fn a_boot_offset_beyond_twice_the_restore_cap_takes_the_boot_step_at_start_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(PL_GM, d, 4)).expect("written");
    // A wall 11 s off UTC (a stale RTC) cannot be on a fleet line kept within 5 s of UTC.
    let clock = boot_step_clock(1, 11_000_000, 1);
    let c = started(clock, ntp_at(11_000_000), &path);
    assert!(
        !c.boot_step_deferred(),
        "the saved state is dropped at start"
    );
    // Within twice the cap (6 s) it is still restored at the first lock.
    let clock = boot_step_clock(0, 6_000_000, 1);
    let c = started(clock, ntp_at(6_000_000), &path);
    assert!(c.boot_step_deferred());
}

#[test]
fn a_restore_inside_a_steps_lead_schedules_the_step_on_the_masters_own_wall_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    // Saved during a step's lead: seq 5 lands +250 ms 8 s from now.
    let mut saved = saved_state(PL_GM, d, 5);
    let now_ptp = wall_now_ns() - d;
    saved.authority.pending = Some((d + 250 * MS, now_ptp + 8 * S));
    restart_file::write_atomic(&path, &saved).expect("written");
    let mut clock = MockSystemClock::new();
    clock.expect_step_clock().times(0); // nothing now: at the instant, with the fleet
    let mut m = started(clock, ntp_at(BOOT_ERROR_US), &path);
    first_lock(&mut m, PL_GM, d);
    m.service_date_offset();
    let own = m
        .date_sync
        .follower
        .pending()
        .expect("the master scheduled the saved step on its own wall");
    assert_eq!((own.seq, own.delta_ns), (5, 250 * MS));
    let st = status(&m);
    assert!(st.date_offset_restored);
    assert_eq!(st.date_offset_seq, Some(5));
    assert_eq!(st.date_step_pending_ns, Some(250 * MS));
    assert_eq!(st.date_offset_ns, Some(d), "in effect only at the instant");
}

#[test]
fn a_node_starting_as_a_follower_removes_a_leftover_saved_state_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let d = wall_now_ns() - PL_PTP_NOW_NS;
    restart_file::write_atomic(&path, &saved_state(PL_GM, d, 4)).expect("written");
    let (mut f, _) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        false,
        restart_config(),
    );
    f.remove_stale_date_state(&path);
    assert!(
        !path.exists(),
        "another session's by the time it is the master again"
    );
    f.remove_stale_date_state(&path); // none: nothing to do
}

#[test]
fn the_master_rewrites_its_saved_state_every_10_minutes_126() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, _) = anchored_controller_with(
        MockSystemClock::new(),
        MockNtpSource::new(),
        true,
        restart_config(),
    );
    c.date_sync.restart.path = Some(path.clone());
    c.service_date_offset();
    let first = restart_file::read(&path)
        .expect("readable")
        .expect("written");
    c.service_date_offset();
    let again = restart_file::read(&path).expect("readable").expect("kept");
    assert_eq!(
        again.written_wall_ns, first.written_wall_ns,
        "unchanged: not rewritten"
    );
    // Ten minutes later it is rewritten unchanged, with a fresh time (a record over a day old is
    // not restored).
    c.date_sync.restart.last_saved_at = Some(Instant::now() - Duration::from_secs(601));
    std::thread::sleep(Duration::from_millis(2));
    c.service_date_offset();
    let later = restart_file::read(&path)
        .expect("readable")
        .expect("rewritten");
    assert_eq!(later.authority, first.authority);
    assert!(later.written_wall_ns > first.written_wall_ns);
}
