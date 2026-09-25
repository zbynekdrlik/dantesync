//! dantesync#88 — the fleet DATE-OFFSET authority and its coordinated-step announce.
//!
//! # The model (owner contract, dantesync#117 ROZHODNUTÉ issuecomment-5836853146)
//!
//! Dante PTP carries no real-world time — the grandmaster's clock is device uptime. Every box
//! phase-locks its system clock to `PTP time + D` (see `crate::ptp_phase_lock`), where `D` is ONE
//! fleet-wide date offset:
//!
//! ```text
//! wall = PTP_time + D            (held by the PTP phase lock: (t2 − t1) − D → 0)
//! ```
//!
//! Only the NTP MASTER reads UTC. It is the single authority for `D`: it keeps `D` such that its
//! wall stays within a step bound (default 50 ms) of UTC, and changes it only when the UTC error
//! exceeds that bound. Each change is announced as `{date_offset_ns, effective_ptp_ns, seq}` with
//! `effective_ptp_ns` at least 5 s in the future, in a versioned extension of the UDP 31900
//! time-query reply (`crate::time_server`). Every box — the master included — applies the step at
//! exactly that PTP instant, so the whole fleet steps TOGETHER instead of each client
//! re-discovering the master's step through its own NTP cadence 10-60 s later (the #88 symptom).
//! Clients NEVER derive the date from their own NTP readings.
//!
//! # What lives here
//!
//! Everything in this module is PURE (explicit time inputs, no I/O, no logging) so the exact code
//! the controller runs is also what the multi-box two-clock bench (`tests/two_clock_bench.rs`)
//! drives:
//!
//! - the wire codec of the 31900 reply extension ([`DateExtension`], [`encode_extension`],
//!   [`decode_extension`]);
//! - [`DateAuthority`] — the master's policy (when to announce, rebase on a GM change);
//! - [`DateFollower`] — every box's step scheduler (join, schedule, apply at the instant, late
//!   catch-up), used by the master for its own announces too, so there is ONE apply path.
//!
//! All times are nanoseconds. "PTP time" is the grandmaster's timestamp domain (`t1`), "wall" is
//! the local system clock (`t2`); on a phase-locked box `wall ≈ ptp + D`.

/// Version of the 31900 reply extension carried by this build.
pub const EXT_VERSION: u8 = 1;

/// Size of the v1 extension appended after the 64-byte base reply.
///
/// Layout (big-endian, offsets relative to the start of the extension = byte 64 of the reply):
///
/// ```text
/// [0]      version (1)
/// [1]      flags: bit 0 = this node is the date-offset AUTHORITY (the NTP master)
/// [2-3]    reserved (zero)
/// [4-11]   date_offset_ns   (i64) — D, where wall = PTP time + D
/// [12-19]  effective_ptp_ns (i64) — the PTP instant D takes effect (future = a pending step)
/// [20-23]  seq              (u32) — bumped on every change of D
/// ```
///
/// A future version APPENDS fields; a v1 reader decodes the first 24 bytes of any version ≥ 1.
pub const EXT_SIZE: usize = 24;

/// Extension flag: the replying node is the fleet's date-offset authority.
pub const EXT_FLAG_AUTHORITY: u8 = 0x01;

/// Default step bound: the master changes `D` only when |UTC − wall| exceeds this (ROZHODNUTÉ Q3).
pub const DEFAULT_STEP_BOUND_NS: i64 = 50_000_000;

/// Minimum announce lead: a step takes effect at least this far in the future (ROZHODNUTÉ Q3).
/// Every client polls the authority once per second, so 5 s gives ≥ 4 chances to hear it.
pub const MIN_STEP_LEAD_NS: i64 = 5_000_000_000;

/// Consecutive same-sign over-bound UTC readings the master needs before it announces. The bound
/// is 50 ms, far above any real NTP noise (WAN bursts scatter by ~1 ms), so this only guards
/// against a single wild reading (a mis-set upstream answering once) moving the whole fleet.
pub const AUTHORITY_AGREEMENT_N: u32 = 2;

/// A follower whose own `D` differs from the authority's by at most this ADOPTS the authority's
/// value without stepping (the PTP phase lock pulls the residual in at a fraction of a ppm).
/// Larger differences are stepped. 100 µs is above the re-anchor noise after a grandmaster
/// change (median of a PTP window, tens of µs) and far below anything a genlock grid notices.
pub const ABSORB_TOLERANCE_NS: i64 = 100_000;

/// One announced date offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateAnnounce {
    /// `D` — wall = PTP time + D.
    pub date_offset_ns: i64,
    /// The PTP instant `D` takes effect. In the future = a pending coordinated step.
    pub effective_ptp_ns: i64,
    /// Bumped on every change of `D` (a step OR a rebase).
    pub seq: u32,
}

/// The decoded 31900 reply extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateExtension {
    pub version: u8,
    /// True only on the fleet's date-offset authority. A client that is not the authority still
    /// mirrors its own state here (observability), with this flag clear — a follower must never
    /// adopt it.
    pub authority: bool,
    pub announce: DateAnnounce,
}

/// Encode the v1 extension (see [`EXT_SIZE`] for the layout).
pub fn encode_extension(ext: &DateExtension) -> [u8; EXT_SIZE] {
    let mut out = [0u8; EXT_SIZE];
    out[0] = EXT_VERSION;
    out[1] = if ext.authority { EXT_FLAG_AUTHORITY } else { 0 };
    out[4..12].copy_from_slice(&ext.announce.date_offset_ns.to_be_bytes());
    out[12..20].copy_from_slice(&ext.announce.effective_ptp_ns.to_be_bytes());
    out[20..24].copy_from_slice(&ext.announce.seq.to_be_bytes());
    out
}

/// Decode an extension. `None` when it is absent (fewer than [`EXT_SIZE`] bytes — an older
/// server's plain 64-byte reply) or when the version byte is 0 (never a valid extension).
/// Any version ≥ 1 is accepted and read as v1: later versions only append fields.
pub fn decode_extension(bytes: &[u8]) -> Option<DateExtension> {
    if bytes.len() < EXT_SIZE || bytes[0] == 0 {
        return None;
    }
    let i64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[at..at + 8]);
        i64::from_be_bytes(b)
    };
    Some(DateExtension {
        version: bytes[0],
        authority: bytes[1] & EXT_FLAG_AUTHORITY != 0,
        announce: DateAnnounce {
            date_offset_ns: i64_at(4),
            effective_ptp_ns: i64_at(12),
            seq: u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
        },
    })
}

// ============================================================================
// THE AUTHORITY (the NTP master)
// ============================================================================

/// The master's date-offset policy. Owns `D` for the whole fleet.
#[derive(Clone, Debug)]
pub struct DateAuthority {
    step_bound_ns: i64,
    lead_ns: i64,
    /// `D` currently in effect.
    current_ns: i64,
    /// The PTP instant `current_ns` took effect.
    current_since_ptp_ns: i64,
    /// A pending coordinated step: (new D, effective PTP instant).
    pending: Option<(i64, i64)>,
    seq: u32,
    /// Consecutive same-sign over-bound readings: (sign, count).
    over_bound: Option<(i8, u32)>,
}

impl DateAuthority {
    /// Establish the authority with the master's own PTP-phase-lock anchor as `D`.
    /// `step_bound_ns` ≤ 0 falls back to the default; `lead_ns` is floored at [`MIN_STEP_LEAD_NS`].
    pub fn new(anchor_ns: i64, now_ptp_ns: i64, step_bound_ns: i64, lead_ns: i64) -> Self {
        DateAuthority {
            step_bound_ns: if step_bound_ns > 0 {
                step_bound_ns
            } else {
                DEFAULT_STEP_BOUND_NS
            },
            lead_ns: lead_ns.max(MIN_STEP_LEAD_NS),
            current_ns: anchor_ns,
            current_since_ptp_ns: now_ptp_ns,
            pending: None,
            seq: 1,
            over_bound: None,
        }
    }

    pub fn step_bound_ns(&self) -> i64 {
        self.step_bound_ns
    }

    pub fn lead_ns(&self) -> i64 {
        self.lead_ns
    }

    pub fn seq(&self) -> u32 {
        self.seq
    }

    /// Promote a pending step whose instant has passed. Called at the top of every entry point,
    /// so the authority's view of "in effect" never lags the clock.
    fn promote(&mut self, now_ptp_ns: i64) {
        if let Some((offset, eff)) = self.pending {
            if eff <= now_ptp_ns {
                self.current_ns = offset;
                self.current_since_ptp_ns = eff;
                self.pending = None;
            }
        }
    }

    /// `D` in effect at `now_ptp_ns`.
    pub fn current_offset_ns(&mut self, now_ptp_ns: i64) -> i64 {
        self.promote(now_ptp_ns);
        self.current_ns
    }

    /// True while a coordinated step is announced but not yet in effect.
    pub fn has_pending(&mut self, now_ptp_ns: i64) -> bool {
        self.promote(now_ptp_ns);
        self.pending.is_some()
    }

    /// What the authority publishes right now: the pending step if one is announced, else the
    /// offset in effect.
    pub fn announce(&mut self, now_ptp_ns: i64) -> DateAnnounce {
        self.promote(now_ptp_ns);
        match self.pending {
            Some((offset, eff)) => DateAnnounce {
                date_offset_ns: offset,
                effective_ptp_ns: eff,
                seq: self.seq,
            },
            None => DateAnnounce {
                date_offset_ns: self.current_ns,
                effective_ptp_ns: self.current_since_ptp_ns,
                seq: self.seq,
            },
        }
    }

    /// Feed one UTC measurement: `utc_error_ns = UTC − wall` on the master (the NTP offset).
    ///
    /// Announces a new `D = D + utc_error_ns` taking effect `lead` from now when the error has
    /// exceeded the bound on [`AUTHORITY_AGREEMENT_N`] consecutive same-sign readings and no step
    /// is already pending. Returns the new announce when it made one.
    ///
    /// Correct because the master's own wall is `ptp + D`: `UTC − ptp = D + (UTC − wall)`.
    pub fn on_utc_error(&mut self, utc_error_ns: i64, now_ptp_ns: i64) -> Option<DateAnnounce> {
        self.promote(now_ptp_ns);
        if self.pending.is_some() {
            // One step at a time. Readings taken while a step is pending describe the wall that
            // is about to move; they must not start a second, stale candidate.
            self.over_bound = None;
            return None;
        }
        if utc_error_ns.abs() <= self.step_bound_ns {
            self.over_bound = None;
            return None;
        }
        let sign: i8 = if utc_error_ns > 0 { 1 } else { -1 };
        let count = match self.over_bound {
            Some((s, n)) if s == sign => n + 1,
            _ => 1,
        };
        if count < AUTHORITY_AGREEMENT_N {
            self.over_bound = Some((sign, count));
            return None;
        }
        self.over_bound = None;
        let eff = now_ptp_ns.saturating_add(self.lead_ns);
        self.pending = Some((self.current_ns.saturating_add(utc_error_ns), eff));
        self.seq = self.seq.wrapping_add(1);
        Some(self.announce(now_ptp_ns))
    }

    /// Move `D` WITHOUT a coordinated step, effective immediately: a re-anchor after a
    /// grandmaster change (the wall is continuous, only the PTP time base moved), or a local
    /// step the master already applied outside the coordinated path (PTP-offline fallback).
    ///
    /// A pending step survives a rebase with its WALL instant and its size unchanged: both the
    /// pending offset and its effective PTP instant are shifted into the new base
    /// (`wall = ptp + D` ⇒ a PTP instant maps to `eff − shift`).
    pub fn rebase(&mut self, new_offset_ns: i64, now_ptp_ns: i64) -> DateAnnounce {
        self.promote(now_ptp_ns);
        let shift = new_offset_ns.wrapping_sub(self.current_ns);
        self.current_ns = new_offset_ns;
        self.current_since_ptp_ns = now_ptp_ns;
        if let Some((offset, eff)) = self.pending {
            self.pending = Some((offset.wrapping_add(shift), eff.wrapping_sub(shift)));
        }
        self.over_bound = None;
        self.seq = self.seq.wrapping_add(1);
        self.announce(now_ptp_ns)
    }
}

// ============================================================================
// THE FOLLOWER (every box, the master included)
// ============================================================================

/// Why a step is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// The first adoption of the authority's `D` (boot, or the authority just appeared). The
    /// only uncoordinated step a healthy follower ever makes.
    Join,
    /// A coordinated step applied at the announced instant — every box together.
    Coordinated,
    /// An announce that was already in effect when it was first heard (the follower missed the
    /// whole lead window). Applied at once and COUNTED: in a healthy fleet this is 0.
    Late,
}

/// What a follower must do in response to an announce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowAction {
    /// Nothing to change.
    None,
    /// Adopt `new_anchor_ns` as `D` without stepping (|difference| ≤ [`ABSORB_TOLERANCE_NS`]).
    Absorb { new_anchor_ns: i64 },
    /// Step the wall by `delta_ns` NOW and set `D += delta_ns`.
    Step { delta_ns: i64, kind: StepKind },
    /// A coordinated step was scheduled; [`DateFollower::due`] returns it at the instant.
    Scheduled {
        delta_ns: i64,
        effective_wall_ns: i64,
    },
}

/// A step waiting for its instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledStep {
    pub seq: u32,
    /// Wall step to apply (and the same shift to `D`).
    pub delta_ns: i64,
    /// Apply when the local wall reaches this. Kept in the WALL domain on purpose: it is
    /// independent of the PTP time base, so a grandmaster change (re-anchor) between the
    /// announce and the instant does not move it.
    pub effective_wall_ns: i64,
}

/// A step that became due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DueStep {
    pub seq: u32,
    pub delta_ns: i64,
}

/// Every box's date-offset step scheduler.
#[derive(Clone, Debug, Default)]
pub struct DateFollower {
    /// The last authority `seq` this box is aligned with. `None` = never adopted (not joined).
    adopted_seq: Option<u32>,
    pending: Option<ScheduledStep>,
    late_steps: u32,
}

impl DateFollower {
    pub fn new() -> Self {
        Self::default()
    }

    /// Has this box ever aligned with the authority?
    pub fn adopted(&self) -> bool {
        self.adopted_seq.is_some()
    }

    pub fn adopted_seq(&self) -> Option<u32> {
        self.adopted_seq
    }

    pub fn pending(&self) -> Option<ScheduledStep> {
        self.pending
    }

    /// Announces that were first heard after their instant (each was applied late).
    pub fn late_steps(&self) -> u32 {
        self.late_steps
    }

    /// Forget the authority alignment (the authority is gone for good, or this box stopped
    /// following). A pending step is dropped with it.
    pub fn forget(&mut self) {
        self.adopted_seq = None;
        self.pending = None;
    }

    /// Handle an announce from the authority. `own_anchor_ns` is this box's `D`, `now_wall_ns`
    /// its wall. The caller must first run [`due`](Self::due) for the same instant, so a step
    /// whose instant has just passed is applied through the coordinated path, not reported late.
    pub fn on_announce(
        &mut self,
        a: DateAnnounce,
        own_anchor_ns: i64,
        now_wall_ns: i64,
    ) -> FollowAction {
        let own_now_ptp = now_wall_ns.wrapping_sub(own_anchor_ns);
        if a.effective_ptp_ns > own_now_ptp {
            // A pending coordinated step. Only a box already aligned with the authority can
            // schedule it: `delta` is relative to the authority's offset in effect, which this
            // box holds only once it has adopted. An unaligned box waits and joins once it is in
            // effect.
            if self.adopted_seq.is_none() {
                return FollowAction::None;
            }
            let step = ScheduledStep {
                seq: a.seq,
                delta_ns: a.date_offset_ns.wrapping_sub(own_anchor_ns),
                effective_wall_ns: a.effective_ptp_ns.wrapping_add(own_anchor_ns),
            };
            if self.pending == Some(step) {
                return FollowAction::None;
            }
            self.pending = Some(step);
            return FollowAction::Scheduled {
                delta_ns: step.delta_ns,
                effective_wall_ns: step.effective_wall_ns,
            };
        }

        // In effect.
        let diff = a.date_offset_ns.wrapping_sub(own_anchor_ns);
        let first = self.adopted_seq.is_none();
        let already = self.adopted_seq == Some(a.seq);
        self.adopted_seq = Some(a.seq);
        // Any scheduled step is superseded by an offset that is already in effect.
        self.pending = None;
        if diff == 0 {
            return FollowAction::None;
        }
        if diff.abs() <= ABSORB_TOLERANCE_NS {
            return FollowAction::Absorb {
                new_anchor_ns: a.date_offset_ns,
            };
        }
        let kind = if first {
            StepKind::Join
        } else if already {
            // Same seq, but this box's D moved away by more than the tolerance (e.g. a local
            // fallback step while the authority was unreachable). Re-join.
            StepKind::Join
        } else {
            self.late_steps = self.late_steps.saturating_add(1);
            StepKind::Late
        };
        FollowAction::Step {
            delta_ns: diff,
            kind,
        }
    }

    /// The scheduled step, once the local wall has reached its instant.
    pub fn due(&mut self, now_wall_ns: i64) -> Option<DueStep> {
        match self.pending {
            Some(p) if now_wall_ns >= p.effective_wall_ns => {
                self.pending = None;
                self.adopted_seq = Some(p.seq);
                Some(DueStep {
                    seq: p.seq,
                    delta_ns: p.delta_ns,
                })
            }
            _ => None,
        }
    }

    /// Nanoseconds until the scheduled step (negative = overdue), if one is scheduled.
    pub fn time_to_due_ns(&self, now_wall_ns: i64) -> Option<i64> {
        self.pending
            .map(|p| p.effective_wall_ns.wrapping_sub(now_wall_ns))
    }
}

#[cfg(test)]
mod tests {
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
            },
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
            assert_eq!(bytes.len(), EXT_SIZE);
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
        assert_eq!(bytes[0], 1, "version byte");
        assert_eq!(bytes[1], EXT_FLAG_AUTHORITY, "authority flag");
        assert_eq!(&bytes[2..4], &[0, 0], "reserved");
        assert_eq!(&bytes[4..12], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            &bytes[12..20],
            &[0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18]
        );
        assert_eq!(&bytes[20..24], &[0x21, 0x22, 0x23, 0x24]);
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
            a.announce(11 * S),
            DateAnnounce {
                date_offset_ns: 900 * S,
                effective_ptp_ns: 10 * S,
                seq: 1
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
                seq: 2
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
        let r = a.rebase(12 * S, 50 * S);
        assert_eq!(
            r,
            DateAnnounce {
                date_offset_ns: 12 * S,
                effective_ptp_ns: 50 * S,
                seq: 2
            }
        );
        assert!(!a.has_pending(50 * S));
    }

    // ---- follower --------------------------------------------------------------------------

    fn in_effect(offset: i64, seq: u32) -> DateAnnounce {
        DateAnnounce {
            date_offset_ns: offset,
            effective_ptp_ns: 0,
            seq,
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
    fn forget_drops_the_alignment_and_any_pending_step() {
        let mut f = DateFollower::new();
        f.on_announce(in_effect(S, 1), S, 5 * S);
        f.on_announce(
            DateAnnounce {
                date_offset_ns: S + 60 * MS,
                effective_ptp_ns: 10 * S,
                seq: 2,
            },
            S,
            6 * S,
        );
        f.forget();
        assert!(!f.adopted());
        assert_eq!(f.due(100 * S), None);
    }
}
