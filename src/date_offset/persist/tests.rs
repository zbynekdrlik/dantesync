//! dantesync#126 — the saved date-offset record: `D` in effect at an instant, and whether a
//! restarted master may restore it.

use super::*;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;
const GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0c];
const OTHER_GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x0a, 0x0b, 0x0d];
/// The fleet line: wall = PTP + D.
const D: i64 = 1_789_274_439_109_968_443;
/// The emergency cap (5 s, the daily default).
const CAP: i64 = 5_000 * MS;

fn state(pending: Option<(i64, i64)>) -> DateOffsetState {
    DateOffsetState {
        authority: AuthorityState {
            d_ns: D,
            since_ptp_ns: 3 * 86_400 * S,
            seq: 7,
            pending,
            slew: None,
            micro: false,
            daily_last_step: Some((D + 4 * 86_400 * S, 1_520 * MS)),
        },
        gm_uuid: GM,
        // A minute before the probes' "now" (PTP time 5 days).
        written_wall_ns: D + 5 * 86_400 * S - 60 * S,
        written_ptp_ns: 5 * 86_400 * S - 60 * S,
    }
}

/// A wall and the anchor of the first locked window of a master whose wall is `off` off the line.
fn first_window(ptp: i64, off: i64) -> (i64, i64) {
    let wall = ptp + D + off;
    (wall, wall - ptp)
}

#[test]
fn a_pending_step_is_in_effect_from_its_instant_126() {
    let st = state(Some((D + 250 * MS, 100 * S)));
    assert_eq!(st.authority.d_in_effect_at(100 * S - 1), D);
    assert_eq!(st.authority.d_in_effect_at(100 * S), D + 250 * MS);
    assert_eq!(state(None).authority.d_in_effect_at(100 * S), D);
}

#[test]
fn the_same_grandmaster_and_a_wall_on_the_line_restores_126() {
    let st = state(None);
    let ptp = 5 * 86_400 * S;
    for off in [0, 37_000, -2 * MS, 1_200 * MS, -CAP, CAP] {
        let (wall, anchor) = first_window(ptp, off);
        assert_eq!(
            st.validate_restore(Some(GM), anchor, wall, CAP),
            Ok(off),
            "a wall {off} ns off the line (a restart's free-run, a host reboot's RTC) is re-joined \
             by the master alone"
        );
    }
}

#[test]
fn the_offset_is_judged_against_the_d_in_effect_at_that_instant_126() {
    // Saved inside a step's lead; the master is back after the instant, its wall where it was
    // (it never took the step): it is the whole step off the fleet line, and re-joins it.
    let st = state(Some((D + 250 * MS, 100 * S)));
    let (wall, anchor) = first_window(200 * S, 0);
    assert_eq!(
        st.validate_restore(Some(GM), anchor, wall, CAP),
        Ok(-250 * MS)
    );
}

/// dantesync#129 (slice 0): the grandmaster UUID is report-only. Another UUID under the same
/// time base (another port of one clock) restores; a grandmaster with another uptime is refused by
/// the time base (`a_time_base_off_beyond_the_cap_is_never_restored_126`); no identity at all is
/// still never restored.
#[test]
fn another_grandmaster_uuid_is_reported_and_none_is_never_restored_129() {
    let st = state(None);
    let (wall, anchor) = first_window(5 * 86_400 * S, 0);
    assert_eq!(
        st.validate_restore(Some(OTHER_GM), anchor, wall, CAP),
        Ok(0)
    );
    assert!(st.names_another_grandmaster(OTHER_GM));
    assert!(!st.names_another_grandmaster(GM));
    assert_eq!(
        st.validate_restore(None, anchor, wall, CAP),
        Err(RestoreRejected::NoGrandmaster)
    );
    let mut legacy = state(None);
    legacy.gm_uuid = crate::ptp::LEGACY_MISREAD_GM_UUID;
    assert!(
        !legacy.names_another_grandmaster(GM),
        "a pre-1.17 record names no grandmaster"
    );
}

#[test]
fn a_time_base_off_beyond_the_cap_is_never_restored_126() {
    let st = state(None);
    let ptp = 5 * 86_400 * S;
    // One ns beyond the cap, either way.
    for off in [CAP + 1, -CAP - 1] {
        let (wall, anchor) = first_window(ptp, off);
        assert_eq!(
            st.validate_restore(Some(GM), anchor, wall, CAP),
            Err(RestoreRejected::TimeBase {
                off_ns: off,
                cap_ns: CAP
            })
        );
    }
    // The grandmaster rebooted under the same identity: its uptime restarted at 42 s, so the
    // first window reads the wall days off the saved D.
    let wall = D + 5 * 86_400 * S;
    let anchor = wall - 42 * S;
    assert!(matches!(
        st.validate_restore(Some(GM), anchor, wall, CAP),
        Err(RestoreRejected::TimeBase { .. })
    ));
}

#[test]
fn a_saved_state_older_than_a_day_is_not_restored_126() {
    let st = state(None);
    let ptp = 5 * 86_400 * S;
    let (wall, anchor) = first_window(ptp, 0);
    assert_eq!(st.validate_restore(Some(GM), anchor, wall, CAP), Ok(0));
    // The same wall line a day and a second after the record was written.
    let later = 86_400 * S + S;
    let (wall, anchor) = first_window(ptp - 60 * S + later, 0);
    assert_eq!(
        st.validate_restore(Some(GM), anchor, wall, CAP),
        Err(RestoreRejected::Stale { age_ns: later })
    );
    // Exactly a day old still restores; a record "from the future" (the wall behind the one that
    // wrote it, a reboot's RTC) is judged by the time base alone.
    let (wall, anchor) = first_window(ptp - 60 * S + 86_400 * S, 0);
    assert!(st.validate_restore(Some(GM), anchor, wall, CAP).is_ok());
    let (wall, anchor) = first_window(ptp - 90 * S, -3 * S);
    assert!(
        wall < st.written_wall_ns,
        "a wall behind the one that wrote the record"
    );
    assert_eq!(st.validate_restore(Some(GM), anchor, wall, CAP), Ok(-3 * S));
}

/// dantesync#129: a 1.16 master saved the constant it misread as the grandmaster's UUID. On the
/// upgrade's restart the live UUID is real, and refusing the restore would boot-step the master to
/// UTC: the fleet date would move. A saved constant names no grandmaster, so the time base decides.
#[test]
fn a_saved_pre_1_17_misread_constant_restores_under_the_real_grandmaster_129() {
    let mut st = state(None);
    st.gm_uuid = crate::ptp::LEGACY_MISREAD_GM_UUID;
    let ptp = 5 * 86_400 * S;
    let (wall, anchor) = first_window(ptp, 37_000);
    assert_eq!(
        st.validate_restore(Some(GM), anchor, wall, CAP),
        Ok(37_000),
        "the same time base restores"
    );
    let (wall, anchor) = first_window(ptp, 10 * S);
    assert_eq!(
        st.validate_restore(Some(GM), anchor, wall, CAP),
        Err(RestoreRejected::TimeBase {
            off_ns: 10 * S,
            cap_ns: CAP
        }),
        "the time-base check still decides"
    );
    assert_eq!(
        st.validate_restore(None, anchor, wall, CAP),
        Err(RestoreRejected::NoGrandmaster),
        "still no restore with no identity at the lock"
    );
}
