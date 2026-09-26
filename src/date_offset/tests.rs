use super::*;

pub(super) const S: i64 = 1_000_000_000;
pub(super) const MS: i64 = 1_000_000;

pub(super) fn ext(offset: i64, eff: i64, seq: u32, authority: bool) -> DateExtension {
    DateExtension {
        version: EXT_VERSION,
        authority,
        announce: DateAnnounce {
            date_offset_ns: offset,
            effective_ptp_ns: eff,
            seq,
            slew: None,
            micro: false,
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
    assert_eq!(
        EXT_VERSION, 3,
        "the #119 micro-corrections bumped the extension to v3"
    );
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
    a.on_utc_error(155 * MS, S);
    a.on_utc_error(155 * MS, 2 * S);
    assert_eq!(a.pending_step_ns(6 * S), Some(155 * MS));
    assert_eq!(a.in_effect_ns(6 * S), 0);
    assert_eq!(
        a.pending_step_ns(7 * S),
        None,
        "at the instant it is no longer pending"
    );
    assert_eq!(a.in_effect_ns(7 * S), 155 * MS);
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
            micro: false,
        }
    );
    assert!(!a.has_pending(11 * S));
}

#[test]
fn a_reading_within_the_cap_never_announces_by_itself_119() {
    // #119 follow-up: a reading up to the abnormal cap (2 × the 50 ms bound) only feeds the
    // micro-correction estimate; the increments come from `on_tick`.
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    for i in 0..100 {
        assert_eq!(
            a.on_utc_error(if i % 2 == 0 { 99 * MS } else { -100 * MS }, i * S),
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
    assert_eq!(a.on_utc_error(-160 * MS, 3 * S), None);
    assert_eq!(
        a.on_utc_error(160 * MS, 4 * S),
        None,
        "opposite sign restarts the count"
    );
    assert_eq!(a.seq(), 1);
}

#[test]
fn authority_announces_the_full_correction_lead_ahead_on_two_agreeing_readings() {
    // #119 follow-up: only an ABNORMAL error (beyond 2 × the bound) is corrected at once.
    let mut a = DateAuthority::new(1_000 * S, 0, 50 * MS, MIN_STEP_LEAD_NS);
    assert_eq!(a.on_utc_error(151 * MS, 100 * S), None);
    let got = a
        .on_utc_error(152 * MS, 110 * S)
        .expect("second agreeing reading announces");
    assert_eq!(
        got,
        DateAnnounce {
            date_offset_ns: 1_000 * S + 152 * MS,
            effective_ptp_ns: 110 * S + MIN_STEP_LEAD_NS,
            seq: 2,
            slew: None,
            micro: false,
        }
    );
    // Until the instant the OLD offset stays in effect.
    assert_eq!(a.current_offset_ns(114 * S), 1_000 * S);
    assert!(a.has_pending(114 * S));
    // At the instant it takes over.
    assert_eq!(a.current_offset_ns(115 * S), 1_000 * S + 152 * MS);
    assert!(!a.has_pending(115 * S));
}

#[test]
fn authority_holds_one_step_at_a_time() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    a.on_utc_error(-170 * MS, S);
    assert!(a.on_utc_error(-170 * MS, 2 * S).is_some());
    // Readings during the lead describe a wall that is about to move: judged by the error left
    // once it has landed, so nothing more is announced.
    assert_eq!(a.on_utc_error(-170 * MS, 3 * S), None);
    assert_eq!(a.on_utc_error(-170 * MS, 4 * S), None);
    assert_eq!(a.on_tick(4 * S), None);
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
    a.on_utc_error(180 * MS, 100 * S);
    let pend = a.on_utc_error(180 * MS, 101 * S).unwrap();
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
            micro: false,
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

pub(super) fn in_effect(offset: i64, seq: u32) -> DateAnnounce {
    DateAnnounce {
        date_offset_ns: offset,
        effective_ptp_ns: 0,
        seq,
        slew: None,
        micro: false,
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
        micro: false,
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
        micro: false,
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
            micro: false,
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
            micro: false,
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
            micro: false,
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

// ---- dantesync#119 follow-up: the MICRO kind ------------------------------------------------

const US: i64 = 1_000;

#[test]
fn a_micro_correction_rides_the_extension_as_v3_and_a_v2_reader_ignores_the_flag_119() {
    let mut e = ext(5 * S + 500 * US, 123 * S, 9, true);
    e.announce.micro = true;
    let bytes = encode_extension(&e);
    assert_eq!(bytes.len(), EXT_SIZE_V2, "v3 keeps the v2 size");
    assert_eq!(bytes[0], 3);
    assert_eq!(bytes[1], EXT_FLAG_AUTHORITY | EXT_FLAG_MICRO);
    assert_eq!(decode_extension(&bytes), Some(e));
    // A micro SLEW carries both flags.
    let mut sl = ext(5 * S - 500 * US, 123 * S, 10, true);
    sl.announce.micro = true;
    sl.announce.slew = Some(SlewSpec {
        from_ns: 5 * S,
        ppm: 100,
    });
    let bytes_sl = encode_extension(&sl);
    assert_eq!(
        bytes_sl[1],
        EXT_FLAG_AUTHORITY | EXT_FLAG_SLEW | EXT_FLAG_MICRO
    );
    assert_eq!(decode_extension(&bytes_sl), Some(sl));
    // A version-2 writer never set bit 2: a v2 extension is never micro, and everything else in
    // it is what a 1.10 follower reads (the same D, instant, seq and slew).
    let mut v2 = bytes_sl;
    v2[0] = 2;
    let old = decode_extension(&v2).unwrap().announce;
    assert!(!old.micro);
    assert_eq!(old.as_slew(), sl.announce.as_slew());
    assert_eq!(old.seq, 10);
    // A plain announce never sets it.
    assert_eq!(encode_extension(&ext(1, 2, 3, true))[1], EXT_FLAG_AUTHORITY);
}

#[test]
fn a_follower_knows_a_micro_step_is_in_flight_until_it_lands_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let micro_step = DateAnnounce {
        date_offset_ns: d + 500 * US,
        effective_ptp_ns: 20 * S,
        seq: 2,
        slew: None,
        micro: true,
    };
    assert_eq!(
        f.on_announce(micro_step, d, d + 15 * S),
        FollowAction::Scheduled {
            delta_ns: 500 * US,
            effective_wall_ns: d + 20 * S
        },
        "applied exactly like any coordinated step"
    );
    assert!(f.is_micro_seq(2));
    assert!(!f.is_micro_seq(1));
    assert!(f.micro_in_flight(d, d + 15 * S));
    let due = f.due(d + 20 * S).unwrap();
    assert_eq!(due.delta_ns, 500 * US);
    assert!(f.is_micro_seq(due.seq));
    assert!(!f.micro_in_flight(d + 500 * US, d + 20 * S + 500 * US));
    // A later plain announce (a rebase, a large correction) is not micro.
    let plain = DateAnnounce {
        date_offset_ns: d + 3 * S,
        effective_ptp_ns: 40 * S,
        seq: 3,
        slew: None,
        micro: false,
    };
    f.on_announce(plain, d + 500 * US, d + 30 * S);
    assert!(!f.is_micro_seq(3));
    assert!(!f.micro_in_flight(d + 500 * US, d + 30 * S));
}

#[test]
fn a_follower_knows_a_held_micro_slew_until_it_is_complete_119() {
    let mut f = DateFollower::new();
    let d = 100 * S;
    f.on_announce(in_effect(d, 1), d, d + 10 * S);
    let micro_slew = DateAnnounce {
        date_offset_ns: d - 500 * US,
        effective_ptp_ns: 20 * S,
        seq: 2,
        slew: Some(SlewSpec {
            from_ns: d,
            ppm: 100,
        }),
        micro: true,
    };
    assert!(matches!(
        f.on_announce(micro_slew, d, d + 15 * S),
        FollowAction::SlewScheduled {
            amount_ns: -500_000,
            ..
        }
    ));
    assert!(f.held_slew_is_micro());
    assert!(f.micro_in_flight(d, d + 15 * S), "scheduled");
    assert!(f.micro_in_flight(d, d + 22 * S), "running");
    // Complete at PTP 25 s (wall d + 25 s − 500 µs, + 1 µs past the ns-floored schedule's last
    // instant): no longer in flight, folded once.
    let end_wall = d + 25 * S - 500 * US + US;
    assert!(!f.micro_in_flight(d, end_wall));
    assert_eq!(f.take_completed_slew(d, end_wall), Some(-500 * US));
    assert!(!f.held_slew_is_micro());
}

#[test]
fn the_effective_micro_interval_covers_the_increment_in_flight_119() {
    let c = MicroConfig::default();
    // Two 5 s leads + a 5 s slew of 500 µs at 100 ppm = 15 s < 20 s: unchanged.
    assert_eq!(effective_micro(c, MIN_STEP_LEAD_NS, 100), c);
    // A 30 s lead: one increment is in flight 60 s + 5 s, so the capacity is honest about it.
    let long = effective_micro(c, 30 * S, 100);
    assert_eq!(long.interval_ns, 65 * S);
    assert!(long.capacity_ns_per_min() < c.capacity_ns_per_min());
    // A 10 ppm slew: 500 µs take 50 s.
    assert_eq!(effective_micro(c, MIN_STEP_LEAD_NS, 10).interval_ns, 60 * S);
    // The authority runs on it, whichever builder comes last.
    assert_eq!(
        DateAuthority::new(0, 0, 50 * MS, 30 * S)
            .micro()
            .config()
            .interval_ns,
        65 * S
    );
    let a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS)
        .with_micro(MicroConfig::new(500, 20))
        .with_slew_ppm(10);
    assert_eq!(a.micro().config().interval_ns, 60 * S);
    let b = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS)
        .with_slew_ppm(10)
        .with_micro(MicroConfig::new(500, 20));
    assert_eq!(b.micro().config().interval_ns, 60 * S);
}

#[test]
fn a_rebase_re_announces_an_in_flight_micro_correction_as_micro_119() {
    let mut a = DateAuthority::new(0, 0, 50 * MS, MIN_STEP_LEAD_NS);
    for i in 0..6 {
        a.on_utc_error(7 * MS, 10 * S + i * 10 * S);
    }
    assert!(a.on_tick(60 * S).unwrap().micro);
    let r = a.rebase(5 * S, 61 * S);
    assert!(
        r.micro,
        "the pending micro step keeps its kind in the new base"
    );
    assert_eq!(r.date_offset_ns - 5 * S, 500 * US, "same size");
    // Once it has landed, a rebase is its own (non-micro) change of D.
    let r2 = a.rebase(10 * S, 100 * S);
    assert!(!r2.micro);
}
