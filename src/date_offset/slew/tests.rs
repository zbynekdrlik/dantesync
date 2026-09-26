//! dantesync#119 — the slew law's tests: the direction decision, the schedule, the v2 wire, the
//! authority's slew policy and every box's hold / join / freeze / fold. Beside
//! `crate::date_offset::tests` (whose helpers they reuse) so neither file outgrows a reader.

use super::super::tests::{ext, in_effect, MS, S};
use super::super::*;

// ---- dantesync#119: a backward correction is a SLEW -----------------------------------------

const US: i64 = 1_000;

fn slew(from: i64, to: i64, start: i64, ppm: u32) -> DateSlew {
    DateSlew {
        from_ns: from,
        to_ns: to,
        start_ptp_ns: start,
        ppm,
    }
}

#[test]
fn the_direction_decides_step_forward_slew_backward_119() {
    let bound = DEFAULT_STEP_BOUND_NS;
    assert_eq!(correction_kind(51 * MS, bound), CorrectionKind::Step);
    assert_eq!(correction_kind(1, bound), CorrectionKind::Step);
    assert_eq!(correction_kind(0, bound), CorrectionKind::Step);
    assert_eq!(correction_kind(3 * S, bound), CorrectionKind::Step);
    assert_eq!(correction_kind(-1, bound), CorrectionKind::Slew);
    assert_eq!(correction_kind(-51 * MS, bound), CorrectionKind::Slew);
}

#[test]
fn the_slew_rate_defaults_to_100_ppm_and_is_clamped_to_10_500_119() {
    assert_eq!(clamp_slew_ppm(0), DEFAULT_SLEW_PPM);
    assert_eq!(DEFAULT_SLEW_PPM, 100);
    assert_eq!(clamp_slew_ppm(1), MIN_SLEW_PPM);
    assert_eq!(clamp_slew_ppm(9), 10);
    assert_eq!(clamp_slew_ppm(10), 10);
    assert_eq!(clamp_slew_ppm(250), 250);
    assert_eq!(clamp_slew_ppm(500), 500);
    assert_eq!(clamp_slew_ppm(501), MAX_SLEW_PPM);
    assert_eq!(clamp_slew_ppm(u32::MAX), 500);
}

#[test]
fn a_slew_pays_its_amount_at_the_rate_and_lands_exactly_on_its_end_119() {
    // 50 ms backward at 100 ppm: 500 s.
    let d = 1_000 * S;
    let sl = slew(d, d - 50 * MS, 200 * S, 100);
    assert_eq!(sl.amount_ns(), -50 * MS);
    assert_eq!(sl.duration_ns(), 500 * S);
    assert_eq!(sl.end_ptp_ns(), 700 * S);
    // Before and at the start nothing moved.
    assert_eq!(sl.offset_at(0), d);
    assert_eq!(sl.offset_at(200 * S), d);
    assert!(!sl.active_at(200 * S - 1));
    assert!(sl.active_at(200 * S));
    // 100 ppm = 100 µs per second.
    assert_eq!(sl.offset_at(201 * S), d - 100 * US);
    assert_eq!(sl.offset_at(300 * S), d - 10 * MS);
    assert_eq!(sl.remaining_ns(300 * S), 40 * MS);
    assert_eq!(sl.rate_ppm_at(300 * S), -100.0);
    // Not a nanosecond early, exact at the end, and held there.
    assert_ne!(sl.offset_at(700 * S - 1), d - 50 * MS);
    assert_eq!(sl.offset_at(700 * S), d - 50 * MS);
    assert_eq!(sl.offset_at(9_000 * S), d - 50 * MS);
    assert!(!sl.active_at(700 * S));
    assert!(sl.complete_at(700 * S));
    assert!(!sl.complete_at(700 * S - 1));
    assert_eq!(sl.rate_ppm_at(700 * S), 0.0);
    assert_eq!(sl.remaining_ns(700 * S), 0);
    // D never moves up during a backward slew, and never faster than the rate.
    let mut prev = sl.offset_at(199 * S);
    for k in 0..6_000 {
        let p = 199 * S + k * 100 * MS + 37;
        let o = sl.offset_at(p);
        assert!(o <= prev, "a backward slew never moves D up");
        assert!(prev - o <= 10 * US + 1, "≤ 100 ppm of 100 ms");
        prev = o;
    }
}

#[test]
fn an_odd_amount_still_ends_exactly_on_to_119() {
    let sl = slew(0, -(51 * MS + 7), 0, 37);
    let end = sl.end_ptp_ns();
    assert_eq!(sl.offset_at(end), -(51 * MS + 7));
    assert!(sl.offset_at(end - 1) > -(51 * MS + 7));
    assert_eq!(
        sl.duration_ns(),
        (((51 * MS + 7) as i128 * 1_000_000 + 36) / 37) as i64
    );
}

#[test]
fn a_shifted_slew_runs_at_the_same_wall_instants_119() {
    let d = 10_000 * S;
    let sl = slew(d, d - 50 * MS, 100 * S, 100);
    let shift = 777 * S; // the new grandmaster's time base: D grows by 777 s
    let moved = sl.shifted(shift);
    for p_old in [50 * S, 100 * S, 150 * S, 600 * S, 900 * S] {
        let wall = p_old + sl.offset_at(p_old);
        let p_new = p_old - shift;
        assert_eq!(p_new + moved.offset_at(p_new), wall);
    }
}

#[test]
fn a_slew_rides_the_extension_as_v2_and_round_trips_119() {
    let mut e = ext(5 * S - 51 * MS, 123 * S, 9, true);
    e.announce.slew = Some(SlewSpec {
        from_ns: 5 * S,
        ppm: 100,
    });
    let bytes = encode_extension(&e);
    assert_eq!(bytes.len(), EXT_SIZE_V2);
    assert_eq!(bytes[1], EXT_FLAG_AUTHORITY | EXT_FLAG_SLEW);
    assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 100);
    assert_eq!(i64::from_be_bytes(bytes[40..48].try_into().unwrap()), 5 * S);
    assert_eq!(decode_extension(&bytes), Some(e));
    assert_eq!(
        e.announce.as_slew(),
        Some(slew(5 * S, 5 * S - 51 * MS, 123 * S, 100))
    );
    // A v1 reader reads the first 40 bytes: a (backward) step, the pre-#119 behaviour.
    let v1 = decode_extension(&bytes[..EXT_SIZE]).expect("v1 part decodes");
    assert_eq!(v1.announce.slew, None);
    assert_eq!(v1.announce.date_offset_ns, 5 * S - 51 * MS);
    // A version-1 writer never set bit 1; even if a byte says so, a v1 extension is no slew.
    let mut v1_flagged = bytes;
    v1_flagged[0] = 1;
    assert_eq!(decode_extension(&v1_flagged).unwrap().announce.slew, None);
}

#[test]
fn a_received_slew_rate_is_clamped_like_a_configured_one_119() {
    let mut e = ext(-S, 0, 1, true);
    e.announce.slew = Some(SlewSpec {
        from_ns: 0,
        ppm: 100,
    });
    let mut bytes = encode_extension(&e);
    bytes[2..4].copy_from_slice(&0u16.to_be_bytes());
    assert_eq!(
        decode_extension(&bytes).unwrap().announce.slew.unwrap().ppm,
        DEFAULT_SLEW_PPM
    );
    bytes[2..4].copy_from_slice(&60_000u16.to_be_bytes());
    assert_eq!(
        decode_extension(&bytes).unwrap().announce.slew.unwrap().ppm,
        MAX_SLEW_PPM
    );
}

/// #119 follow-up: six readings of `error`, ten seconds apart, ending at `end` — enough for the
/// micro estimate. The corrections themselves come from `on_tick`.
fn feed(a: &mut DateAuthority, error: i64, end: i64) {
    for i in 0..6 {
        assert_eq!(a.on_utc_error(error, end - (5 - i) * 10 * S), None);
    }
}

#[test]
fn a_negative_correction_is_announced_as_a_micro_slew_never_a_backward_step_119() {
    let d = 1_000 * S;
    let mut a = DateAuthority::new(d, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -7 * MS, 110 * S);
    let ann = a
        .on_tick(110 * S)
        .expect("beyond the dead band: an increment");
    assert_eq!(
        ann,
        DateAnnounce {
            date_offset_ns: d - 500 * US,
            effective_ptp_ns: 110 * S + MICRO_LEAD_FACTOR * MIN_STEP_LEAD_NS,
            seq: 2,
            slew: Some(SlewSpec {
                from_ns: d,
                ppm: DEFAULT_SLEW_PPM
            }),
            micro: true,
        }
    );
    // No step is ever pending for it …
    for p in [111 * S, 114 * S, 115 * S, 118 * S, 200 * S] {
        assert_eq!(a.pending_step_ns(p), None);
    }
    // … D follows the slew: unchanged until the start (two leads ahead), then 100 µs/s down,
    // then the target.
    assert_eq!(a.in_effect_ns(119 * S), d);
    assert_eq!(a.in_effect_ns(122 * S), d - 200 * US);
    assert!(a.slew_in_progress(122 * S).is_some());
    let end = 120 * S + 5 * S;
    assert_eq!(a.in_effect_ns(end), d - 500 * US);
    assert_eq!(a.current_offset_ns(end), d - 500 * US);
    assert!(a.slew_in_progress(end).is_none());
    // Promoted: the plain offset in effect, same seq (D did not change again), still micro.
    assert_eq!(
        a.announce(),
        DateAnnounce {
            date_offset_ns: d - 500 * US,
            effective_ptp_ns: end,
            seq: 2,
            slew: None,
            micro: true,
        }
    );
}

#[test]
fn a_positive_correction_is_a_micro_step_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, 7 * MS, 60 * S);
    let ann = a.on_tick(60 * S).unwrap();
    assert_eq!(ann.slew, None);
    assert!(ann.micro);
    assert_eq!(a.pending_step_ns(61 * S), Some(500 * US));
    assert!(a.slew_in_progress(61 * S).is_none());
    assert_eq!(a.in_effect_ns(69 * S), 0, "two leads ahead");
    assert_eq!(a.in_effect_ns(70 * S), 500 * US);
}

#[test]
fn the_authority_slews_at_its_configured_rate_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS).with_slew_ppm(250);
    assert_eq!(a.slew_ppm(), 250);
    feed(&mut a, -7 * MS, 60 * S);
    let ann = a.on_tick(60 * S).unwrap();
    assert_eq!(ann.slew.unwrap().ppm, 250);
    assert_eq!(ann.as_slew().unwrap().duration_ns(), 2 * S);
    assert_eq!(
        DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS)
            .with_slew_ppm(3)
            .slew_ppm(),
        MIN_SLEW_PPM
    );
}

#[test]
fn the_authority_takes_its_configured_micro_step_and_interval_119() {
    let mut a =
        DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS).with_micro(MicroConfig::new(200, 30));
    feed(&mut a, 7 * MS, 60 * S);
    assert_eq!(a.on_tick(60 * S).unwrap().date_offset_ns, 200 * US);
    assert_eq!(a.on_tick(89 * S), None, "30 s apart");
    assert_eq!(a.on_tick(90 * S).unwrap().date_offset_ns, 400 * US);
    assert_eq!(a.micro().config(), MicroConfig::new(200, 30));
}

#[test]
fn a_micro_slew_in_progress_absorbs_the_readings_it_is_already_paying_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -3 * MS, 60 * S);
    let first = a.on_tick(60 * S).unwrap().as_slew().unwrap();
    // The wall has not moved yet (scheduled) or is part-way down: the reading still shows the
    // −3 ms, minus what is already paid; judged by the error left once the slew has paid
    // (−2.5 ms), nothing new is announced while it runs.
    for p in [61 * S, 63 * S, 66 * S, 68 * S, 69 * S] {
        let reading = -3 * MS - a.in_effect_ns(p);
        assert_eq!(a.on_utc_error(reading, p), None);
        assert_eq!(a.on_tick(p), None);
    }
    assert_eq!(a.seq(), 2);
    assert_eq!(
        a.micro().estimate(first.end_ptp_ns()).unwrap().error_ns,
        -2_500 * US,
        "the kept readings describe the error left after the increment"
    );
}

#[test]
fn the_next_increment_waits_for_the_running_one_and_the_interval_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -9 * MS, 60 * S);
    let first = a.on_tick(60 * S).unwrap().as_slew().unwrap();
    assert_eq!(first.end_ptp_ns(), 75 * S);
    assert_eq!(
        a.on_tick(76 * S),
        None,
        "the slew ended, the 20 s interval did not"
    );
    let second = a.on_tick(80 * S).unwrap().as_slew().unwrap();
    assert_eq!(
        second.from_ns,
        -500 * US,
        "continues from where the first ended"
    );
    assert_eq!(second.to_ns, -MS);
    assert_eq!(second.start_ptp_ns, 90 * S);
    assert_eq!(a.seq(), 3);
}

#[test]
fn a_forward_need_beyond_the_cap_during_a_slew_waits_for_its_end_and_is_stepped_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -3 * MS, 60 * S);
    let first = a.on_tick(60 * S).unwrap().as_slew().unwrap();
    let p = first.start_ptp_ns + S;
    // UTC jumped FORWARD by 200 ms: an abnormal error, beyond the 100 ms cap.
    let r = -3 * MS - a.in_effect_ns(p) + 200 * MS;
    assert_eq!(a.on_utc_error(r, p), None);
    assert_eq!(a.on_utc_error(r, p + S), None, "no step during a slew");
    assert_eq!(
        a.on_tick(p + S),
        None,
        "no micro increment while it is confirmed"
    );
    assert_eq!(a.seq(), 2);
    let end = first.end_ptp_ns();
    let ann = a
        .on_utc_error(197 * MS + 500 * US, end + S)
        .expect("stepped after it");
    assert_eq!(ann.slew, None);
    assert!(!ann.micro, "the abnormal correction is not a micro one");
    assert_eq!(ann.date_offset_ns, -500 * US + 197 * MS + 500 * US);
}

#[test]
fn a_rebase_moves_a_running_slew_into_the_new_base_119() {
    let d = 1_000 * S;
    let mut a = DateAuthority::new(d, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -3 * MS, 60 * S);
    let first = a.on_tick(60 * S).unwrap().as_slew().unwrap();
    let p_old = first.start_ptp_ns + 2 * S;
    let wall = p_old + a.in_effect_ns(p_old);
    let shift = 500 * S;
    let r = a.rebase(a.in_effect_ns(p_old) + shift, p_old);
    let moved = r.as_slew().expect("still the slew");
    assert_eq!(r.seq, 3);
    assert!(
        !r.micro,
        "a rebase is its own change of D, not a micro-correction"
    );
    assert_eq!(moved, first.shifted(shift));
    // Same wall line now and at every later instant.
    for dt in [0, S, 3 * S, 10 * S] {
        let p_old_t = p_old + dt;
        let wall_t = p_old_t + first.offset_at(p_old_t);
        let p_new_t = p_old_t - shift;
        assert_eq!(p_new_t + a.in_effect_ns(p_new_t), wall_t, "dt {dt}");
    }
    assert_eq!(wall, p_old + first.offset_at(p_old));
    // The micro estimate moved into the new base with it.
    assert_eq!(
        a.micro().estimate(p_old - shift).unwrap().error_ns,
        -2_500 * US
    );
}

// ---- dantesync#119: every box follows the slew -----------------------------------------------

/// Equal to the nanosecond. The slew's schedule is floored to whole ns, so at a nanosecond
/// boundary two adjacent PTP instants give the same wall: recovering PTP time (and so `D`) from a
/// wall reading is exact to 1 ns, never more.
fn assert_ns(got: i64, want: i64) {
    assert!((got - want).abs() <= 1, "got {got}, want {want} (± 1 ns)");
}

/// A slew announce of `from → to` starting at PTP `start`.
fn slew_announce(from: i64, to: i64, start: i64, seq: u32) -> DateAnnounce {
    slew(from, to, start, 100).announce(seq)
}

#[test]
fn a_joined_follower_schedules_a_slew_and_moves_d_only_from_its_start_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let ann = slew_announce(d, d - 50 * MS, 20 * S, 2);
    assert_eq!(
        f.on_announce(ann, d, d + 15 * S),
        FollowAction::SlewScheduled {
            amount_ns: -50 * MS,
            start_wall_ns: d + 20 * S,
            ppm: 100
        }
    );
    assert_eq!(f.adopted_seq(), Some(2));
    // Before the start: no displacement, no rate.
    assert_eq!(f.in_effect_ns(d, d + 19 * S), d);
    assert_eq!(f.slew_rate_ppm(d, d + 19 * S), 0.0);
    // Running: the wall follows PTP + D(PTP), D(PTP) is the slew's schedule.
    let wall = d + 20 * S + 100 * S - 10 * MS; // PTP 120 s, D = d − 10 ms
    assert_ns(f.in_effect_ns(d, wall), d - 10 * MS);
    assert_ns(f.now_ptp_ns(d, wall), 120 * S);
    assert_eq!(f.slew_rate_ppm(d, wall), -100.0);
    assert_ns(f.slew_remaining_ns(d, wall).unwrap(), 40 * MS);
    assert_eq!(f.take_completed_slew(d, wall), None);
    // Re-announced every poll: nothing to do (at most a 1 ns absorb, see `assert_ns`).
    assert!(matches!(
        f.on_announce(ann, d, wall),
        FollowAction::None
            | FollowAction::Absorb {
                new_anchor_ns: 99_999_999_999..=100_000_000_001
            }
    ));
    // Complete at PTP 520 s: folded exactly once, D continuous across the fold.
    let wall_end = d + 520 * S - 50 * MS + US;
    assert_eq!(f.in_effect_ns(d, wall_end), d - 50 * MS);
    assert_eq!(f.slew_rate_ppm(d, wall_end), 0.0);
    assert_eq!(f.take_completed_slew(d, wall_end), Some(-50 * MS));
    assert_eq!(f.take_completed_slew(d, wall_end), None);
    assert_eq!(f.in_effect_ns(d - 50 * MS, wall_end), d - 50 * MS);
    // The authority's promoted form afterwards: nothing to do.
    assert_eq!(
        f.on_announce(
            DateAnnounce {
                date_offset_ns: d - 50 * MS,
                effective_ptp_ns: 520 * S,
                seq: 2,
                slew: None,
                micro: false,
            },
            d - 50 * MS,
            wall_end + S
        ),
        FollowAction::None
    );
    // … nor the slew form read after its end.
    assert_eq!(
        f.on_announce(ann, d - 50 * MS, wall_end + 2 * S),
        FollowAction::None
    );
    assert_eq!(f.late_steps(), 0);
}

#[test]
fn a_box_that_joins_mid_slew_lands_on_the_current_d_and_slews_only_the_rest_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    let own = d + 3 * MS; // boot NTP left it 3 ms off, never joined
    let ann = slew_announce(d, d - 50 * MS, 20 * S, 7);
    // Still scheduled: an unjoined box waits.
    assert_eq!(f.on_announce(ann, own, own + 15 * S), FollowAction::None);
    // 200 s in (20 ms paid): one Join onto the fleet's current D …
    let wall = own + 220 * S;
    let p = f.now_ptp_ns(own, wall);
    let fleet_now = ann.as_slew().unwrap().offset_at(p);
    let act = f.on_announce(ann, own, wall);
    assert_eq!(
        act,
        FollowAction::Step {
            delta_ns: fleet_now - own,
            kind: StepKind::Join
        }
    );
    let anchor = own + (fleet_now - own);
    // … then it follows the fleet's schedule exactly (only the remaining ~30 ms).
    let sl = ann.as_slew().unwrap();
    for dt in [0, S, 100 * S, 300 * S] {
        let p2 = p + dt;
        let w2 = p2 + sl.offset_at(p2);
        assert_ns(f.in_effect_ns(anchor, w2), sl.offset_at(p2));
    }
    let h = f.held_slew().unwrap();
    assert_eq!(
        h.total_displacement(),
        sl.to_ns - fleet_now,
        "the remaining part"
    );
    assert_eq!(f.late_steps(), 0);
}

#[test]
fn a_joined_box_that_missed_the_whole_lead_catches_up_as_a_counted_late_step_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let ann = slew_announce(d, d - 50 * MS, 20 * S, 2);
    // First heard 30 s after the start: the fleet already paid 3 ms.
    let act = f.on_announce(ann, d, d + 50 * S);
    assert_eq!(
        act,
        FollowAction::Step {
            delta_ns: -3 * MS,
            kind: StepKind::Late
        }
    );
    assert_eq!(f.late_steps(), 1);
}

#[test]
fn an_extension_heard_after_its_start_changes_nothing_but_the_end_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let first = slew(d, d - 60 * MS, 20 * S, 100);
    f.on_announce(first.announce(2), d, d + 15 * S);
    // The authority extends at PTP 120 s; this box hears it at PTP 121.3 s.
    let ext = slew(first.offset_at(120 * S), d - 140 * MS, 120 * S, 100);
    let p = 121 * S + 300 * MS;
    let wall = p + first.offset_at(p);
    assert_ns(f.now_ptp_ns(d, wall), p);
    assert!(matches!(
        f.on_announce(ext.announce(3), d, wall),
        FollowAction::None | FollowAction::Absorb { .. }
    ));
    // It now runs to the extended end, from exactly where it was.
    let later = 2_000 * S; // past its end at PTP 1 420 s
    assert_eq!(
        f.in_effect_ns(d, later + ext.offset_at(later)),
        d - 140 * MS
    );
    assert!((f.in_effect_ns(d, wall) - first.offset_at(p)).abs() <= 1);
    assert_eq!(f.late_steps(), 0);
}

#[test]
fn a_plain_announce_while_a_slew_runs_freezes_it_where_it_is_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    f.on_announce(slew_announce(d, d - 50 * MS, 20 * S, 2), d, d + 15 * S);
    let p = 120 * S; // 10 ms paid
    let wall = p + d - 10 * MS;
    // A restarted authority re-establishes D (seq 1) at the fleet's current line.
    let act = f.on_announce(in_effect(d - 10 * MS, 1), d, wall);
    assert!(
        matches!(
            act,
            FollowAction::None
                | FollowAction::Absorb {
                    new_anchor_ns: 99_999_999_999..=100_000_000_001
                }
        ),
        "D was already there: {act:?}"
    );
    assert_eq!(f.slew_rate_ppm(d, wall + S), 0.0, "no longer slewing");
    assert_ns(f.take_completed_slew(d, wall).expect("folded"), -10 * MS);
}

#[test]
fn freeze_keeps_d_continuous_and_rebase_keeps_the_wall_instants_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let sl = slew(d, d - 50 * MS, 20 * S, 100);
    f.on_announce(sl.announce(2), d, d + 15 * S);
    // Rebase: D grows by 3 days; the same wall instants run the same displacement.
    let shift = 3 * 86_400 * S;
    let wall = 120 * S + sl.offset_at(120 * S);
    let before = f.displacement_at_wall(d, wall);
    f.rebase_slew(shift);
    assert_eq!(f.displacement_at_wall(d + shift, wall), before);
    assert_ns(f.now_ptp_ns(d + shift, wall), 120 * S - shift);
    // Freeze: same D now, no rate after.
    let d_now = f.in_effect_ns(d + shift, wall);
    f.freeze_slew(d + shift, wall);
    assert_eq!(f.in_effect_ns(d + shift, wall), d_now);
    assert_eq!(f.in_effect_ns(d + shift, wall + 100 * S), d_now);
    assert_eq!(f.slew_rate_ppm(d + shift, wall + S), 0.0);
}

#[test]
fn every_box_holding_the_same_slew_has_the_same_d_at_the_same_ptp_instant_119() {
    // Three boxes whose anchors kept different absorbed residuals: each one's D moves by exactly
    // the slew's schedule of PTP time, and the fixed-point solve recovers PTP time from the wall.
    let d = 1_000 * S;
    let sl = slew(d, d - 51 * MS, 50 * S, 100);
    for residual in [0, 40 * US, -70 * US] {
        let anchor = d + residual;
        let mut f = DateFollower::new();
        f.on_announce(in_effect(anchor, 1), anchor, anchor + 10 * S);
        f.on_announce(sl.announce(2), anchor, anchor + 20 * S);
        for k in 0..700 {
            let p = 40 * S + k * S + 123_456;
            let own_d = anchor + sl.offset_at(p) - d;
            let wall = p + own_d;
            assert_eq!(
                f.in_effect_ns(anchor, wall),
                own_d,
                "residual {residual} k {k}"
            );
            assert_eq!(f.now_ptp_ns(anchor, wall), p);
        }
    }
}

#[test]
fn the_solved_d_is_exactly_the_schedule_at_the_solved_ptp_instant_119() {
    // The master's own "on the fleet line" test compares its D in effect with the authority's
    // schedule at the PTP instant derived from that D: they must agree to the NANOSECOND, for
    // any rate and amount (a 1 ns miss at such an instant used to skip the master's own
    // scheduling of an extension).
    for (ppm, amount) in [
        (100u32, 51 * MS),
        (100, S),
        (500, 51 * MS),
        (500, S),
        (10, 3 * S),
    ] {
        let d = 1_790_000_000 * S;
        let sl = slew(d, d - amount, 1_000 * S, ppm);
        let mut f = DateFollower::new();
        f.on_announce(in_effect(d, 1), d, d + 10 * S);
        f.on_announce(sl.announce(2), d, d + 20 * S);
        let mut wall = d + 900 * S;
        let step = sl.duration_ns() / 50_000 + 7_919;
        for _ in 0..60_000 {
            let own = f.in_effect_ns(d, wall);
            assert_eq!(
                sl.offset_at(wall - own),
                own,
                "ppm {ppm} amount {amount} wall {wall}"
            );
            wall += step;
        }
    }
}

#[test]
fn the_promoted_form_heard_just_before_this_boxs_own_end_changes_nothing_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let sl = slew(d, d - 50 * MS, 20 * S, 100);
    f.on_announce(sl.announce(2), d, d + 15 * S);
    // The authority promoted at its own end (PTP 520 s); this box is 3 µs short of it.
    let promoted = DateAnnounce {
        date_offset_ns: d - 50 * MS,
        effective_ptp_ns: sl.end_ptp_ns(),
        seq: 2,
        slew: None,
        micro: false,
    };
    let p = sl.end_ptp_ns() - 3 * US;
    let wall = p + sl.offset_at(p);
    assert_eq!(f.on_announce(promoted, d, wall), FollowAction::None);
    assert_eq!(f.pending(), None, "never a step, however small");
    assert!(f.held_slew().is_some(), "the slew runs on to its end");
    let wall_end = sl.end_ptp_ns() + sl.to_ns + US;
    assert_eq!(f.take_completed_slew(d, wall_end), Some(-50 * MS));
}

#[test]
fn a_forward_slew_from_the_wire_is_not_a_slew_119() {
    // Only a backward correction is ever slewed. A slew part whose end is not below its start
    // (not sent by any authority of ours) is read as the plain announce: a forward step.
    let mut e = ext(5 * S + 51 * MS, 123 * S, 9, true);
    e.announce.slew = Some(SlewSpec {
        from_ns: 5 * S,
        ppm: 100,
    });
    assert_eq!(e.announce.as_slew(), None);
    e.announce.date_offset_ns = 5 * S;
    assert_eq!(e.announce.as_slew(), None, "a zero-length one neither");
    e.announce.date_offset_ns = 5 * S - 1;
    assert!(e.announce.as_slew().is_some());
}

// ---- dantesync#119 ROZHODNUTÉ issuecomment-5842590141: the slew cap ----------------------------

#[test]
fn a_backward_correction_beyond_twice_the_bound_is_too_large_to_slew_119() {
    let bound = DEFAULT_STEP_BOUND_NS; // 50 ms → the cap is 100 ms
    assert_eq!(slew_cap_ns(bound), 100 * MS);
    assert_eq!(
        correction_kind(-100 * MS, bound),
        CorrectionKind::Slew,
        "at the cap: slew"
    );
    assert_eq!(
        correction_kind(-100 * MS - 1, bound),
        CorrectionKind::TooLargeToSlew,
        "one ns past it: a step"
    );
    assert_eq!(
        correction_kind(-3 * S, bound),
        CorrectionKind::TooLargeToSlew
    );
    // The cap follows the configured bound.
    assert_eq!(
        correction_kind(-41 * MS, 20 * MS),
        CorrectionKind::TooLargeToSlew
    );
    assert_eq!(correction_kind(-40 * MS, 20 * MS), CorrectionKind::Slew);
}

#[test]
fn a_master_booted_3_s_ahead_steps_the_fleet_back_60_ms_ahead_is_micro_slewed_119() {
    // −3 s (a boot-time NTP failure): an abnormal state, one coordinated step, never hours of
    // micro-corrections.
    let d = 1_000 * S;
    let mut a = DateAuthority::new(d, 0, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-3 * S, 10 * S);
    let ann = a.on_utc_error(-3 * S, 20 * S).expect("announced");
    assert_eq!(ann.slew, None, "not a slew");
    assert!(!ann.micro);
    assert_eq!(ann.date_offset_ns, d - 3 * S);
    assert_eq!(ann.effective_ptp_ns, 20 * S + MIN_STEP_LEAD_NS);
    assert_eq!(a.pending_step_ns(21 * S), Some(-3 * S));
    assert!(a.slew_in_progress(21 * S).is_none());
    assert_eq!(
        a.micro().estimate(21 * S),
        None,
        "the micro history starts again"
    );
    // −60 ms (within the cap): only ever micro-slewed, 500 µs at a time.
    let mut b = DateAuthority::new(d, 0, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS);
    feed(&mut b, -60 * MS, 60 * S);
    let ann = b.on_tick(60 * S).expect("announced");
    assert_eq!(ann.as_slew().unwrap().amount_ns(), -500 * US);
    assert_eq!(b.pending_step_ns(61 * S), None);
    // A joined follower schedules the too-large one as a coordinated step at its instant.
    let mut f = DateFollower::new();
    f.on_announce(in_effect(d, 1), d, d + 15 * S);
    let big = DateAnnounce {
        date_offset_ns: d - 3 * S,
        effective_ptp_ns: 25 * S,
        seq: 2,
        slew: None,
        micro: false,
    };
    assert_eq!(
        f.on_announce(big, d, d + 21 * S),
        FollowAction::Scheduled {
            delta_ns: -3 * S,
            effective_wall_ns: d + 25 * S
        }
    );
}

#[test]
fn a_backward_need_beyond_the_cap_during_a_slew_is_stepped_after_it_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    feed(&mut a, -3 * MS, 60 * S);
    let first = a.on_tick(60 * S).unwrap().as_slew().unwrap();
    // 1 s in, UTC jumps back 250 ms: the need left once the slew has paid is beyond the cap.
    let p = first.start_ptp_ns + S;
    let r = -3 * MS - a.in_effect_ns(p) - 250 * MS;
    assert_eq!(a.on_utc_error(r, p), None);
    assert_eq!(a.on_utc_error(r, p + S), None, "never slewed");
    assert_eq!(a.seq(), 2);
    let end = first.end_ptp_ns();
    let ann = a
        .on_utc_error(-252_500 * US, end + S)
        .expect("stepped after it");
    assert_eq!(ann.slew, None);
    assert_eq!(ann.date_offset_ns, -500 * US - 252_500 * US);
}
