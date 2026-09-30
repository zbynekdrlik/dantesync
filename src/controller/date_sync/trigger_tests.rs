//! dantesync#126 — the coordinated date step ON REQUEST, controller side: only the NTP master
//! on the fleet line announces it (the whole current UTC error, one coordinated step two leads
//! ahead, scheduled on its own wall and published at once), a follower schedules it from the
//! published announce, and every refusal says why. The HTTP route's end is
//! `crate::http_status`'s tests; the authority's decision `crate::date_offset`'s.

use super::restart_tests::{ntp_at, restart_config, status};
use super::tests::PL_GM;
use super::tests::{anchored_controller_with, authority_reply, readings_then_tick, with_authority};
use super::*;
use crate::clock::MockSystemClock;
use crate::date_step_trigger::{DateStepOutcome, DateStepRequest};

const MS: i64 = 1_000_000;

fn refused(outcome: DateStepOutcome) -> String {
    match outcome {
        DateStepOutcome::Refused { reason } => reason,
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[test]
fn a_step_on_request_is_one_coordinated_step_the_master_schedules_and_publishes_126() {
    let mut clock = MockSystemClock::new();
    // It lands two leads (10 s) ahead: never inside this test.
    clock.expect_step_clock().times(0);
    let (mut c, d) = anchored_controller_with(clock, ntp_at(300_000), true, restart_config());
    readings_then_tick(&mut c);
    assert_eq!(status(&c).date_step_pending_ns, None, "nothing by day");

    let DateStepOutcome::Accepted {
        amount_ns,
        land_ptp_ns,
        due_in_ms,
        seq,
    } = c.date_step_on_request()
    else {
        panic!("the master on the line accepts");
    };
    assert!(
        (amount_ns - 300 * MS).abs() < MS,
        "the whole current error: {amount_ns}"
    );
    assert!(
        (9_000..=10_000).contains(&due_in_ms),
        "two 5 s leads: {due_in_ms}"
    );
    assert_eq!(seq, 2);
    let st = status(&c);
    assert_eq!(
        st.date_step_pending_ns,
        Some(amount_ns),
        "published at once"
    );
    assert_eq!(st.date_offset_ns, Some(d), "in effect only at the instant");
    assert!(
        st.date_step_trigger_last.contains("accepted"),
        "{}",
        st.date_step_trigger_last
    );
    let own = c
        .date_sync
        .follower
        .pending()
        .expect("the master scheduled it on its own wall");
    assert_eq!((own.seq, own.delta_ns), (2, amount_ns));

    // A follower schedules it from the published announce, for the same instant.
    let (mut f, _) =
        anchored_controller_with(MockSystemClock::new(), ntp_at(0), false, restart_config());
    // On the same fleet line as the master (built a moment later, its own anchor would differ by
    // the real time between the two).
    f.date_sync.core.set_anchor(d);
    let slot = with_authority(&mut f);
    let now_ptp = wall_now_ns() - d;
    *slot.lock().unwrap() = Some(authority_reply(1, PL_GM, d, now_ptp - 1_000_000_000, 1));
    f.service_date_offset();
    *slot.lock().unwrap() = Some(authority_reply(2, PL_GM, d + amount_ns, land_ptp_ns, 2));
    f.service_date_offset();
    let fs = status(&f);
    assert_eq!(
        fs.date_step_pending_ns,
        Some(amount_ns),
        "scheduled, not stepped now"
    );
    assert_eq!(fs.date_steps_late, 0);

    // A second request while it is in flight: refused.
    let reason = refused(c.date_step_on_request());
    assert!(reason.contains("in flight"), "{reason}");
    assert!(status(&c).date_step_trigger_last.contains("refused"));
}

#[test]
fn a_step_on_request_is_refused_off_the_authority_and_for_a_fleet_ahead_of_utc_126() {
    // A follower is never the authority.
    let (mut f, _) =
        anchored_controller_with(MockSystemClock::new(), ntp_at(0), false, restart_config());
    let reason = refused(f.date_step_on_request());
    assert!(
        reason.contains("not the fleet date-offset authority"),
        "{reason}"
    );

    // The master without enough UTC readings.
    let (mut m, _) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(300_000),
        true,
        restart_config(),
    );
    let reason = refused(m.date_step_on_request());
    assert!(reason.contains("no settled UTC estimate"), "{reason}");

    // The fleet ahead of UTC: never stepped back on request.
    let (mut m, _) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(-300_000),
        true,
        restart_config(),
    );
    readings_then_tick(&mut m);
    let reason = refused(m.date_step_on_request());
    assert!(reason.contains("not behind UTC"), "{reason}");
    assert_eq!(m.date_sync.authority.as_ref().map(|a| a.seq()), Some(1));

    // The master without PTP.
    let (mut m, _) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(300_000),
        true,
        restart_config(),
    );
    readings_then_tick(&mut m);
    m.ptp_offline = true;
    let reason = refused(m.date_step_on_request());
    assert!(reason.contains("no PTP"), "{reason}");
}

#[test]
fn the_loop_answers_every_queued_request_on_its_channel_126() {
    let (mut c, _) = anchored_controller_with(
        MockSystemClock::new(),
        ntp_at(300_000),
        true,
        restart_config(),
    );
    readings_then_tick(&mut c);
    let (tx, rx) = crate::date_step_trigger::channel();
    c.set_date_step_requests(rx);
    let (reply1, answer1) = std::sync::mpsc::channel();
    let (reply2, answer2) = std::sync::mpsc::channel();
    tx.send(DateStepRequest { reply: reply1 }).expect("queued");
    tx.send(DateStepRequest { reply: reply2 }).expect("queued");
    c.serve_date_step_requests();
    assert!(matches!(
        answer1.try_recv(),
        Ok(DateStepOutcome::Accepted { .. })
    ));
    assert!(
        matches!(answer2.try_recv(), Ok(DateStepOutcome::Refused { .. })),
        "one change at a time"
    );
    // A request whose asker gave up is still consumed, nothing breaks.
    let (reply3, answer3) = std::sync::mpsc::channel();
    drop(answer3);
    tx.send(DateStepRequest { reply: reply3 }).expect("queued");
    c.serve_date_step_requests();
    assert_eq!(
        c.date_sync.authority.as_ref().map(|a| a.seq()),
        Some(2),
        "still the one step"
    );
}
