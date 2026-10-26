//! dantesync#134 — fast boot sync: the fleet date is placed at the FIRST PTP window, not at the
//! rate servo's lock (~31-40 s later), with ONE step, on the master and on a follower; the PTP
//! loop never waits for the boot NTP reading; and the later engagement of the phase lock never
//! takes a second step. The pure core is proven in `crate::ptp_phase_lock` (the boot hold), the
//! acquisition on two clocks in `tests/two_clock_bench/boot.rs`.

use super::restart_file;
use super::restart_tests::{restart_config, status, Ctl};
use super::tests::{authority_reply, one_offset, with_authority, PL_GM, PL_PTP_NOW_NS};
use super::*;
use crate::clock::MockSystemClock;
use crate::date_offset::{AuthorityState, DateOffsetState};
use crate::traits::{MockNtpSource, MockPtpNetwork};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
/// strih-pp, 25.10.2026: its RTC came back 3.75 s behind the fleet line.
const RTC_BEHIND_MS: i64 = 3_750;

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
    mut clock: MockSystemClock,
    master: bool,
    path: Option<&Path>,
) -> (Ctl, Sender<BootNtpResult>) {
    clock.expect_adjust_frequency().returning(|_| Ok(()));
    let mut c = PtpController::new(
        clock,
        MockPtpNetwork::new(),
        MockNtpSource::new(),
        Arc::new(RwLock::new(SyncStatus::default())),
        restart_config(),
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

fn saved(d: i64, seq: u32) -> DateOffsetState {
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
        gm_uuid: PL_GM,
        written_wall_ns: now - 60 * S,
        written_ptp_ns: now - d - 60 * S,
    }
}

/// A master with a saved state placed on the fleet line at its FIRST window: restored, one step.
fn master_placed_at_the_first_window(
    fleet: i64,
) -> (Ctl, Sender<BootNtpResult>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    restart_file::write_atomic(&path, &saved(fleet, 2)).expect("written");
    let (mut c, tx) = booted(steps_once(RTC_BEHIND_MS * MS), true, Some(&path));
    assert!(c.boot_step_deferred(), "the saved state waits for PTP");
    // The first window: not locked (the rate servo has just started), the wall 3.75 s behind.
    window(&mut c, fleet - RTC_BEHIND_MS * MS, false);
    assert!(!c.date_sync.core.engaged(), "still acquiring");
    let st = status(&c);
    assert_eq!(st.date_authority, "master");
    assert!(st.date_offset_restored, "restored at the first window");
    assert_eq!(st.date_offset_ns, Some(fleet));
    assert_eq!(st.date_offset_seq, Some(2), "the same session");
    assert_eq!(st.date_step_pending_ns, None, "this master is on the line");
    assert_eq!(c.date_sync.core.anchor_ns(), Some(fleet));
    assert_eq!(st.last_date_step_kind, "join");
    (c, tx, dir)
}

#[test]
fn a_master_with_a_saved_state_places_the_date_at_the_first_window_with_one_step_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (mut c, tx, _dir) = master_placed_at_the_first_window(fleet);
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
fn a_master_without_a_saved_state_takes_its_boot_step_from_the_late_reading_then_its_authority_134()
{
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("date-offset.json");
    let (mut c, tx) = booted(steps_once(250 * MS), true, Some(&path));
    let d0 = wall_now_ns() - PL_PTP_NOW_NS;
    window(&mut c, d0, false);
    assert_eq!(
        c.date_sync.core.anchor_ns(),
        Some(d0),
        "anchored at the first window"
    );
    assert!(
        c.date_sync.authority.is_none(),
        "no session on a wall the boot step has not placed yet"
    );
    // The loop's periodic NTP check would block the loop on the same unanswered upstream: it
    // waits for the boot reading (the NTP mock panics if called).
    c.last_ntp_check = Instant::now() - Duration::from_secs(120);
    c.check_ntp_utc_tracking();
    // The reading arrives: the boot step (+250 ms), D moves with the wall …
    tx.send(Ok(one_offset(250_000, 1))).expect("send");
    c.service_boot_ntp();
    assert_eq!(c.date_sync.core.anchor_ns(), Some(d0 + 250 * MS));
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
fn a_follower_joins_the_fleet_line_at_its_first_window_and_never_steps_again_134() {
    let fleet = wall_now_ns() - PL_PTP_NOW_NS;
    let (mut c, tx) = booted(steps_once(40 * MS), false, None);
    let slot = with_authority(&mut c);
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, fleet, PL_PTP_NOW_NS - 600 * S, 2));
    // The first window, 40 ms behind the fleet line, not locked: ONE join step.
    window(&mut c, fleet - 40 * MS, false);
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
    let (mut c, tx, _dir) = master_placed_at_the_first_window(fleet);
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
