use super::*;

const S: i64 = 1_000_000_000;
const MS: i64 = 1_000_000;

fn ext(offset: i64, eff: i64, seq: u32, authority: bool) -> DateExtension {
    DateExtension {
        version: EXT_VERSION,
        authority,
        announce: DateAnnounce {
            date_offset_ns: offset,
            effective_ptp_ns: eff,
            seq,
            slew: None,
        },
        gm_uuid: [0x00, 0x1d, 0xc1, 0x01, 0x02, (seq & 0xff) as u8],
        now_ptp_ns: eff.wrapping_add(7),
    }
}

// ---- wire codec ------------------------------------------------------------------------

#[test]
fn extension_round_trips_every_field_including_negative_offsets() {
    for e in [
        ext(1_790_000_000 * S, 12_345 * S + 678, 7, true),
        ext(-3 * S - 1, -1, u32::MAX, false),
        ext(i64::MAX, i64::MIN, 0, true),
    ] {
        let bytes = encode_extension(&e);
        assert_eq!(bytes.len(), EXT_SIZE_V2);
        assert_eq!(decode_extension(&bytes), Some(e));
    }
}

#[test]
fn extension_layout_is_the_documented_big_endian_one() {
    let bytes = encode_extension(&ext(
        0x0102_0304_0506_0708,
        0x1112_1314_1516_1718,
        0x2122_2324,
        true,
    ));
    assert_eq!(bytes[0], EXT_VERSION, "version byte");
    assert_eq!(EXT_VERSION, 2, "#119 bumped the extension to v2");
    assert_eq!(bytes[1], EXT_FLAG_AUTHORITY, "authority flag");
    assert_eq!(&bytes[2..4], &[0, 0], "no slew: slew_ppm is zero");
    assert_eq!(&bytes[4..12], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        &bytes[12..20],
        &[0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
    );
    assert_eq!(&bytes[20..24], &[0x21, 0x22, 0x23, 0x24]);
    assert_eq!(
        &bytes[24..30],
        &[0x00, 0x1d, 0xc1, 0x01, 0x02, 0x24],
        "anchor GM"
    );
    assert_eq!(&bytes[30..32], &[0, 0], "reserved");
    assert_eq!(
        i64::from_be_bytes(bytes[32..40].try_into().unwrap()),
        0x1112_1314_1516_1718 + 7,
        "the replier's PTP now"
    );
}

// ---- time-base check -------------------------------------------------------------------

#[test]
fn same_base_nodes_agree_regardless_of_their_wall_error() {
    // Both hear GM time 1000 s. The follower's wall is 3 h off (never joined yet), but its D
    // carries the same error, so its PTP view is the same.
    let gm = 1_000 * S;
    let own_d = 1_789_000_000 * S + 3 * 3_600 * S;
    assert!(same_time_base(gm, gm + own_d + 5 * MS, own_d));
}

#[test]
fn a_multi_second_pending_step_does_not_disturb_the_check() {
    // The master announced a +3 s step: its PUBLISHED D is D + 3 s, but its PTP now comes from
    // the D in effect, so a follower in the same base still recognises it.
    let d = 1_789_000_000 * S;
    let gm = 5_000 * S;
    let master_now_ptp = gm; // its wall (gm + d) − its D in effect (d)
    assert!(same_time_base(master_now_ptp, gm + d + 2 * MS, d));
}

#[test]
fn a_rebooted_or_different_grandmaster_is_a_different_base() {
    let d_old = 1_789_000_000 * S - 3 * 86_400 * S; // GM uptime 3 days
    let d_new = 1_789_000_000 * S - 10 * S; // the same GM after a reboot: uptime 10 s
    let wall = 1_789_000_000 * S;
    assert!(!same_time_base(wall - d_old, wall, d_new));
    assert!(
        !same_time_base(wall - d_new + 2 * S, wall, d_new),
        "2 s apart is not the same view"
    );
}

// ---- authority: read-only views ----------------------------------------------------------

#[test]
fn pending_step_and_in_effect_views_do_not_mutate() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(55 * MS, S);
    a.on_utc_error(55 * MS, 2 * S);
    assert_eq!(a.pending_step_ns(6 * S), Some(55 * MS));
    assert_eq!(a.in_effect_ns(6 * S), 0);
    assert_eq!(
        a.pending_step_ns(7 * S),
        None,
        "at the instant it is no longer pending"
    );
    assert_eq!(a.in_effect_ns(7 * S), 55 * MS);
}

#[test]
fn a_missing_or_truncated_extension_decodes_as_absent() {
    assert_eq!(
        decode_extension(&[]),
        None,
        "an older server's 64-byte reply has none"
    );
    let full = encode_extension(&ext(5, 6, 7, true));
    assert_eq!(decode_extension(&full[..EXT_SIZE - 1]), None);
    let mut zero_version = full;
    zero_version[0] = 0;
    assert_eq!(decode_extension(&zero_version), None);
}

#[test]
fn a_future_version_with_appended_fields_is_read_as_v1() {
    let mut v2 = encode_extension(&ext(42, 43, 44, true)).to_vec();
    v2[0] = 2;
    v2.extend_from_slice(&[0xAA; 16]);
    let got = decode_extension(&v2).expect("v2 must still decode");
    assert_eq!(got.version, 2);
    assert_eq!(got.announce, ext(42, 43, 44, true).announce);
}

// ---- authority -------------------------------------------------------------------------

#[test]
fn authority_publishes_its_anchor_in_effect_at_seq_1() {
    let mut a = DateAuthority::new(900 * S, 10 * S, DEFAULT_STEP_BOUND_NS, MIN_STEP_LEAD_NS);
    assert_eq!(
        a.announce(),
        DateAnnounce {
            date_offset_ns: 900 * S,
            effective_ptp_ns: 10 * S - IMMEDIATE_BACKDATE_NS,
            seq: 1,
            slew: None,
        }
    );
    assert!(!a.has_pending(11 * S));
}

#[test]
fn authority_ignores_errors_within_the_bound() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    for i in 0..100 {
        assert_eq!(
            a.on_utc_error(if i % 2 == 0 { 49 * MS } else { -50 * MS }, i * S),
            None
        );
    }
    assert_eq!(a.seq(), 1);
}

#[test]
fn authority_never_announces_on_a_single_over_bound_reading() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    assert_eq!(
        a.on_utc_error(900 * MS, S),
        None,
        "one wild reading must not move the fleet"
    );
    assert_eq!(
        a.on_utc_error(10 * MS, 2 * S),
        None,
        "back within bound clears the candidate"
    );
    assert_eq!(a.on_utc_error(-60 * MS, 3 * S), None);
    assert_eq!(
        a.on_utc_error(60 * MS, 4 * S),
        None,
        "opposite sign restarts the count"
    );
    assert_eq!(a.seq(), 1);
}

#[test]
fn authority_announces_the_full_correction_lead_ahead_on_two_agreeing_readings() {
    let mut a = DateAuthority::new(1_000 * S, 0, 50 * MS, MIN_STEP_LEAD_NS);
    assert_eq!(a.on_utc_error(51 * MS, 100 * S), None);
    let got = a
        .on_utc_error(52 * MS, 110 * S)
        .expect("second agreeing reading announces");
    assert_eq!(
        got,
        DateAnnounce {
            date_offset_ns: 1_000 * S + 52 * MS,
            effective_ptp_ns: 110 * S + MIN_STEP_LEAD_NS,
            seq: 2,
            slew: None,
        }
    );
    // Until the instant the OLD offset stays in effect.
    assert_eq!(a.current_offset_ns(114 * S), 1_000 * S);
    assert!(a.has_pending(114 * S));
    // At the instant it takes over.
    assert_eq!(a.current_offset_ns(115 * S), 1_000 * S + 52 * MS);
    assert!(!a.has_pending(115 * S));
}

#[test]
fn authority_holds_one_step_at_a_time() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-70 * MS, S);
    assert!(a.on_utc_error(-70 * MS, 2 * S).is_some());
    // Readings during the lead describe a wall that is about to move: ignored.
    assert_eq!(a.on_utc_error(-70 * MS, 3 * S), None);
    assert_eq!(a.on_utc_error(-70 * MS, 4 * S), None);
    assert_eq!(a.seq(), 2);
}

#[test]
fn authority_lead_is_floored_and_bound_defaults() {
    let a = DateAuthority::new(0, 0, 0, 1);
    assert_eq!(a.step_bound_ns(), DEFAULT_STEP_BOUND_NS);
    assert_eq!(a.lead_ns(), MIN_STEP_LEAD_NS);
    let b = DateAuthority::new(0, 0, 20 * MS, 9 * S);
    assert_eq!(b.step_bound_ns(), 20 * MS);
    assert_eq!(b.lead_ns(), 9 * S);
}

#[test]
fn rebase_moves_d_immediately_and_keeps_a_pending_step_at_the_same_wall_instant() {
    let mut a = DateAuthority::new(1_000 * S, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(80 * MS, 100 * S);
    let pend = a.on_utc_error(80 * MS, 101 * S).unwrap();
    let wall_instant = pend.effective_ptp_ns + 1_000 * S;
    let size = pend.date_offset_ns - 1_000 * S;

    // GM change at PTP 102 s: the new grandmaster's time is 500 s behind the old one, so D
    // grows by 500 s while the wall stays continuous.
    let r = a.rebase(1_500 * S, 102 * S);
    assert_eq!(r.seq, pend.seq + 1);
    assert_eq!(
        r.effective_ptp_ns + 1_500 * S,
        wall_instant,
        "same wall instant"
    );
    assert_eq!(r.date_offset_ns - 1_500 * S, size, "same step size");
}

#[test]
fn rebase_without_pending_is_in_effect_now() {
    let mut a = DateAuthority::new(10 * S, 0, 50 * MS, MIN_STEP_LEAD_NS);
    // Now = PTP 50 s in the old base = wall 60 s. D grows by 2 s ⇒ now = PTP 48 s in the new
    // base: the same wall instant.
    let r = a.rebase(12 * S, 50 * S);
    assert_eq!(
        r,
        DateAnnounce {
            date_offset_ns: 12 * S,
            effective_ptp_ns: 48 * S - IMMEDIATE_BACKDATE_NS,
            seq: 2,
            slew: None,
        }
    );
    assert_eq!(
        r.effective_ptp_ns + IMMEDIATE_BACKDATE_NS + r.date_offset_ns,
        60 * S,
        "wall continuous: now in the new base is PTP 48 s = wall 60 s"
    );
    assert!(!a.has_pending(48 * S));
}

// ---- follower --------------------------------------------------------------------------

fn in_effect(offset: i64, seq: u32) -> DateAnnounce {
    DateAnnounce {
        date_offset_ns: offset,
        effective_ptp_ns: 0,
        seq,
        slew: None,
    }
}

#[test]
fn follower_joins_with_one_step_then_stays_quiet() {
    let mut f = DateFollower::new();
    let d_own = 1_000 * S;
    let d_auth = 1_000 * S + 3 * MS; // boot NTP left this box 3 ms off the authority
    assert_eq!(
        f.on_announce(in_effect(d_auth, 4), d_own, 2_000 * S),
        FollowAction::Step {
            delta_ns: 3 * MS,
            kind: StepKind::Join
        }
    );
    assert!(f.adopted());
    assert_eq!(
        f.on_announce(in_effect(d_auth, 4), d_auth, 2_001 * S),
        FollowAction::None
    );
    assert_eq!(f.late_steps(), 0);
}

#[test]
fn follower_absorbs_a_sub_tolerance_difference_without_stepping() {
    let mut f = DateFollower::new();
    assert_eq!(
        f.on_announce(in_effect(5 * S + 40_000, 1), 5 * S, 100 * S),
        FollowAction::Absorb {
            new_anchor_ns: 5 * S + 40_000
        }
    );
    assert_eq!(
        f.on_announce(in_effect(5 * S - ABSORB_TOLERANCE_NS, 1), 5 * S, 100 * S),
        FollowAction::Absorb {
            new_anchor_ns: 5 * S - ABSORB_TOLERANCE_NS
        },
        "the tolerance itself is still absorbed"
    );
}

#[test]
fn an_unjoined_follower_ignores_a_pending_step_until_it_is_in_effect() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    let pending = DateAnnounce {
        date_offset_ns: d + 60 * MS,
        effective_ptp_ns: 50 * S,
        seq: 9,
        slew: None,
    };
    assert_eq!(f.on_announce(pending, d, d + 45 * S), FollowAction::None);
    assert_eq!(f.pending(), None);
    // In effect now → one join.
    assert_eq!(
        f.on_announce(pending, d, d + 51 * S),
        FollowAction::Step {
            delta_ns: 60 * MS,
            kind: StepKind::Join
        }
    );
}

#[test]
fn a_joined_follower_schedules_and_applies_at_the_wall_instant() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let pending = DateAnnounce {
        date_offset_ns: d - 55 * MS,
        effective_ptp_ns: 20 * S,
        seq: 2,
        slew: None,
    };
    assert_eq!(
        f.on_announce(pending, d, d + 15 * S),
        FollowAction::Scheduled {
            delta_ns: -55 * MS,
            effective_wall_ns: d + 20 * S
        }
    );
    assert_eq!(
        f.on_announce(pending, d, d + 16 * S),
        FollowAction::None,
        "idempotent"
    );
    assert_eq!(f.due(d + 20 * S - 1), None, "not a nanosecond early");
    assert_eq!(
        f.due(d + 20 * S),
        Some(DueStep {
            seq: 2,
            delta_ns: -55 * MS
        })
    );
    assert_eq!(f.due(d + 21 * S), None, "applied once");
    assert_eq!(f.adopted_seq(), Some(2));
    // The post-step poll sees the offset in effect and equal to ours: nothing more.
    assert_eq!(
        f.on_announce(in_effect(d - 55 * MS, 2), d - 55 * MS, d + 22 * S),
        FollowAction::None
    );
    assert_eq!(f.late_steps(), 0);
}

#[test]
fn a_follower_that_missed_the_lead_window_steps_late_and_counts_it() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    assert_eq!(
        f.on_announce(in_effect(d + 70 * MS, 2), d, d + 30 * S),
        FollowAction::Step {
            delta_ns: 70 * MS,
            kind: StepKind::Late
        }
    );
    assert_eq!(f.late_steps(), 1);
}

#[test]
fn a_scheduled_step_survives_a_local_re_anchor_in_the_wall_domain() {
    // The follower's own GM changes between the announce and the instant: its D moves by
    // +500 s, the wall does not. The scheduled step must still fire at the same WALL instant
    // with the same size.
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    f.on_announce(
        DateAnnounce {
            date_offset_ns: d + 60 * MS,
            effective_ptp_ns: 20 * S,
            seq: 2,
            slew: None,
        },
        d,
        d + 15 * S,
    );
    // (the re-anchor happens in the phase lock; the follower keeps its wall-domain schedule)
    assert_eq!(f.time_to_due_ns(d + 18 * S), Some(2 * S));
    assert_eq!(
        f.due(d + 20 * S),
        Some(DueStep {
            seq: 2,
            delta_ns: 60 * MS
        })
    );
}

#[test]
fn a_restarted_authority_is_re_joined_not_counted_late() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 57), d, d + 10 * S);
    // The master restarted: seq back to 1, its offset re-established 400 µs away.
    assert_eq!(
        f.on_announce(in_effect(d + 400_000, 1), d, d + 20 * S),
        FollowAction::Step {
            delta_ns: 400_000,
            kind: StepKind::Join
        }
    );
    assert_eq!(f.late_steps(), 0);
    assert_eq!(f.adopted_seq(), Some(1));
}

#[test]
fn cancel_pending_keeps_the_alignment() {
    let mut f = DateFollower::new();
    f.on_announce(in_effect(S, 1), S, 5 * S);
    f.on_announce(
        DateAnnounce {
            date_offset_ns: S + 60 * MS,
            effective_ptp_ns: 10 * S,
            seq: 2,
            slew: None,
        },
        S,
        6 * S,
    );
    f.cancel_pending();
    assert!(f.adopted());
    assert_eq!(f.due(100 * S), None);
}

#[test]
fn forget_drops_the_alignment_but_keeps_a_scheduled_step() {
    let mut f = DateFollower::new();
    f.on_announce(in_effect(S, 1), S, 5 * S);
    f.on_announce(
        DateAnnounce {
            date_offset_ns: S + 60 * MS,
            effective_ptp_ns: 10 * S,
            seq: 2,
            slew: None,
        },
        S,
        6 * S,
    );
    f.forget();
    assert!(!f.adopted());
    // The fleet applies seq 2 at its instant; so must this box, even if it stopped following.
    assert_eq!(
        f.due(11 * S),
        Some(DueStep {
            seq: 2,
            delta_ns: 60 * MS
        })
    );
}

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
    assert_eq!(correction_kind(51 * MS), CorrectionKind::Step);
    assert_eq!(correction_kind(1), CorrectionKind::Step);
    assert_eq!(correction_kind(0), CorrectionKind::Step);
    assert_eq!(correction_kind(-1), CorrectionKind::Slew);
    assert_eq!(correction_kind(-51 * MS), CorrectionKind::Slew);
    assert_eq!(correction_kind(-3 * S), CorrectionKind::Slew);
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

#[test]
fn a_negative_correction_is_announced_as_a_slew_never_a_backward_step_119() {
    let d = 1_000 * S;
    let mut a = DateAuthority::new(d, 0, 50 * MS, MIN_STEP_LEAD_NS);
    assert_eq!(a.on_utc_error(-51 * MS, 100 * S), None);
    let ann = a
        .on_utc_error(-52 * MS, 110 * S)
        .expect("two agreeing readings announce");
    assert_eq!(
        ann,
        DateAnnounce {
            date_offset_ns: d - 52 * MS,
            effective_ptp_ns: 110 * S + MIN_STEP_LEAD_NS,
            seq: 2,
            slew: Some(SlewSpec {
                from_ns: d,
                ppm: DEFAULT_SLEW_PPM
            }),
        }
    );
    // No step is ever pending for it …
    for p in [111 * S, 114 * S, 115 * S, 200 * S, 700 * S] {
        assert_eq!(a.pending_step_ns(p), None);
    }
    // … D follows the slew: unchanged until the start, then 100 µs/s down, then the target.
    assert_eq!(a.in_effect_ns(114 * S), d);
    assert_eq!(a.in_effect_ns(125 * S), d - MS);
    assert!(a.slew_in_progress(125 * S).is_some());
    let end = 115 * S + 520 * S;
    assert_eq!(a.in_effect_ns(end), d - 52 * MS);
    assert_eq!(a.current_offset_ns(end), d - 52 * MS);
    assert!(a.slew_in_progress(end).is_none());
    // Promoted: the plain offset in effect, same seq (D did not change again).
    assert_eq!(
        a.announce(),
        DateAnnounce {
            date_offset_ns: d - 52 * MS,
            effective_ptp_ns: end,
            seq: 2,
            slew: None,
        }
    );
}

#[test]
fn a_positive_correction_is_still_a_coordinated_step_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(60 * MS, S);
    let ann = a.on_utc_error(60 * MS, 2 * S).unwrap();
    assert_eq!(ann.slew, None);
    assert_eq!(a.pending_step_ns(3 * S), Some(60 * MS));
    assert!(a.slew_in_progress(3 * S).is_none());
}

#[test]
fn the_authority_slews_at_its_configured_rate_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS).with_slew_ppm(250);
    assert_eq!(a.slew_ppm(), 250);
    a.on_utc_error(-60 * MS, S);
    let ann = a.on_utc_error(-60 * MS, 2 * S).unwrap();
    assert_eq!(ann.slew.unwrap().ppm, 250);
    assert_eq!(ann.as_slew().unwrap().duration_ns(), 240 * S);
    assert_eq!(
        DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS)
            .with_slew_ppm(3)
            .slew_ppm(),
        MIN_SLEW_PPM
    );
}

#[test]
fn a_slew_in_progress_absorbs_the_readings_it_is_already_paying_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-60 * MS, S);
    a.on_utc_error(-60 * MS, 2 * S).unwrap();
    // The wall has not moved yet (scheduled) or is part-way down: the error left once the slew
    // has paid is ≈ 0, so nothing new is announced.
    for (k, p) in [3 * S, 7 * S, 100 * S, 300 * S, 500 * S].iter().enumerate() {
        let paid = a.in_effect_ns(*p); // ≤ 0
        let reading = -60 * MS - paid + (k as i64) * 100 * US;
        assert_eq!(a.on_utc_error(reading, *p), None);
    }
    assert_eq!(a.seq(), 2);
}

#[test]
fn a_further_backward_need_extends_the_running_slew_continuously_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-60 * MS, S);
    let first = a.on_utc_error(-60 * MS, 2 * S).unwrap().as_slew().unwrap();
    // 100 s into the slew (10 ms paid) UTC jumps back by another 80 ms: the error left once
    // the slew has paid is −80 ms, twice.
    let p1 = first.start_ptp_ns + 100 * S;
    let paid = a.in_effect_ns(p1);
    assert_eq!(paid, -10 * MS);
    let reading = -60 * MS - paid - 80 * MS;
    assert_eq!(a.on_utc_error(reading, p1), None);
    let p2 = p1 + 10 * S;
    let reading2 = -60 * MS - a.in_effect_ns(p2) - 80 * MS;
    let ext = a
        .on_utc_error(reading2, p2)
        .expect("extension")
        .as_slew()
        .unwrap();
    assert_eq!(a.seq(), 3);
    // Continuous: it starts NOW from D now, at the same rate, and ends 80 ms lower.
    assert_eq!(ext.start_ptp_ns, p2);
    assert_eq!(ext.from_ns, first.offset_at(p2));
    assert_eq!(ext.to_ns, -140 * MS);
    assert_eq!(ext.ppm, first.ppm);
    for p in [p2, p2 + S, p2 + 3 * S, p2 + 100 * S] {
        assert!(
            (ext.offset_at(p) - first.offset_at(p)).abs() <= 1,
            "a box that still runs the old slew is on the new one until it hears it"
        );
    }
    assert_eq!(a.in_effect_ns(ext.end_ptp_ns()), -140 * MS);
}

#[test]
fn a_slew_too_close_to_its_end_is_not_extended_the_next_one_follows_it_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-60 * MS, S);
    let first = a.on_utc_error(-60 * MS, 2 * S).unwrap().as_slew().unwrap();
    let end = first.end_ptp_ns();
    // 3 s before its end (< the 5 s lead): a follower might not hear an extension in time.
    let p = end - 3 * S;
    let r = -60 * MS - a.in_effect_ns(p) - 80 * MS;
    assert_eq!(a.on_utc_error(r, p), None);
    assert_eq!(a.on_utc_error(r, p + S), None);
    assert_eq!(a.seq(), 2);
    // After the end the agreed need is a fresh slew, lead ahead.
    let next = a
        .on_utc_error(-80 * MS, end + 2 * S)
        .expect("announced right after the end")
        .as_slew()
        .unwrap();
    assert_eq!(next.from_ns, -60 * MS);
    assert_eq!(next.to_ns, -140 * MS);
    assert_eq!(next.start_ptp_ns, end + 2 * S + MIN_STEP_LEAD_NS);
}

#[test]
fn a_forward_need_during_a_slew_waits_for_its_end_and_is_stepped_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-60 * MS, S);
    let first = a.on_utc_error(-60 * MS, 2 * S).unwrap().as_slew().unwrap();
    let p = first.start_ptp_ns + 100 * S;
    // UTC jumped FORWARD by 200 ms: once the slew has paid the wall would be 140 ms behind.
    let r = -60 * MS - a.in_effect_ns(p) + 200 * MS;
    assert_eq!(a.on_utc_error(r, p), None);
    assert_eq!(a.on_utc_error(r, p + 10 * S), None, "no step during a slew");
    assert_eq!(a.seq(), 2);
    let end = first.end_ptp_ns();
    let ann = a.on_utc_error(140 * MS, end + S).expect("stepped after it");
    assert_eq!(ann.slew, None);
    assert_eq!(ann.date_offset_ns, -60 * MS + 140 * MS);
}

#[test]
fn a_rebase_moves_a_running_slew_into_the_new_base_119() {
    let d = 1_000 * S;
    let mut a = DateAuthority::new(d, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-60 * MS, S);
    let first = a.on_utc_error(-60 * MS, 2 * S).unwrap().as_slew().unwrap();
    let p_old = first.start_ptp_ns + 200 * S;
    let wall = p_old + a.in_effect_ns(p_old);
    let shift = 500 * S;
    let r = a.rebase(a.in_effect_ns(p_old) + shift, p_old);
    let moved = r.as_slew().expect("still the slew");
    assert_eq!(r.seq, 3);
    assert_eq!(moved, first.shifted(shift));
    // Same wall line now and at every later instant.
    for dt in [0, 10 * S, 100 * S, 400 * S] {
        let p_old_t = p_old + dt;
        let wall_t = p_old_t + first.offset_at(p_old_t);
        let p_new_t = p_old_t - shift;
        assert_eq!(p_new_t + a.in_effect_ns(p_new_t), wall_t, "dt {dt}");
    }
    assert_eq!(wall, p_old + first.offset_at(p_old));
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
                slew: None
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
