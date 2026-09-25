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
/// [24-29]  gm_uuid          (6 bytes) — the grandmaster whose PTP time base D belongs to
/// [30-31]  reserved (zero)
/// [32-39]  now_ptp_ns       (i64) — the replying node's PTP time when it built the reply
///                            (its wall − its D IN EFFECT), for the follower's time-base check
/// ```
///
/// `gm_uuid` is the grandmaster of the authority's ANCHOR, not "the grandmaster I hear right now":
/// during a grandmaster change those differ for a window, and publishing the new UUID beside an
/// old-base `D` would let a follower that already re-anchored adopt a days-wrong offset.
///
/// A future version APPENDS fields; a v1 reader decodes the first 40 bytes of any version ≥ 1.
pub const EXT_SIZE: usize = 40;

/// Extension flag: the replying node is the fleet's date-offset authority.
pub const EXT_FLAG_AUTHORITY: u8 = 0x01;

/// Default step bound: the master changes `D` only when |UTC − wall| exceeds this (ROZHODNUTÉ Q3).
pub const DEFAULT_STEP_BOUND_NS: i64 = 50_000_000;

/// Minimum announce lead: a step takes effect at least this far in the future (ROZHODNUTÉ Q3).
/// Every client polls the authority once per second, so 5 s gives ≥ 4 chances to hear it.
pub const MIN_STEP_LEAD_NS: i64 = 5_000_000_000;

/// An offset that takes effect IMMEDIATELY (the first anchor, a rebase onto a new time base) is
/// published with its effective instant this far in the PAST. It marks no wall step to meet — the
/// instant is informational — and a follower whose PTP view of "now" trails the master's by a few
/// µs (path delay, re-anchor noise) must read it as in effect, never as a pending step to schedule.
pub const IMMEDIATE_BACKDATE_NS: i64 = 1_000_000_000;

/// Consecutive same-sign over-bound UTC readings the master needs before it announces. The bound
/// is 50 ms, far above any real NTP noise (WAN bursts scatter by ~1 ms), so this only guards
/// against a single wild reading (a mis-set upstream answering once) moving the whole fleet.
pub const AUTHORITY_AGREEMENT_N: u32 = 2;

/// A reply is only applicable when the replying node's PTP time "now" and this node's agree within
/// this. Two different time bases — another grandmaster, or the same grandmaster after a reboot
/// restarted its uptime under the same UUID — differ by the grandmasters' uptime difference
/// (seconds to days), while two nodes in the same base differ only by the reply's age and the
/// wall disagreement (≪ 1 s).
///
/// Both "now"s are taken from each node's D IN EFFECT — never from a published D that may carry a
/// pending step: an announced step is larger than the step bound by construction and has no
/// upper limit (a master whose boot-time NTP failed announces seconds), so a check fed the pending
/// D would refuse exactly the steps that matter most.
pub const TIME_BASE_TOLERANCE_NS: i64 = 1_000_000_000;

/// Is a reply in this node's PTP time base? `remote_now_ptp_ns` is the replying node's PTP time
/// when it built the reply (the extension's `now_ptp_ns`), `own_wall_ns` this node's wall when the
/// reply arrived, `own_offset_ns` this node's `D` in effect. Independent of either node's wall
/// error, so it holds before a join. See [`TIME_BASE_TOLERANCE_NS`].
pub fn same_time_base(remote_now_ptp_ns: i64, own_wall_ns: i64, own_offset_ns: i64) -> bool {
    let own_ptp = own_wall_ns.wrapping_sub(own_offset_ns);
    remote_now_ptp_ns.wrapping_sub(own_ptp).unsigned_abs() <= TIME_BASE_TOLERANCE_NS as u64
}

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
    /// The grandmaster whose PTP time base `announce.date_offset_ns` belongs to.
    pub gm_uuid: [u8; 6],
    /// The replying node's PTP time when it built the reply (its wall − its `D` in effect).
    pub now_ptp_ns: i64,
}

/// Encode the v1 extension (see [`EXT_SIZE`] for the layout).
pub fn encode_extension(ext: &DateExtension) -> [u8; EXT_SIZE] {
    let mut out = [0u8; EXT_SIZE];
    out[0] = EXT_VERSION;
    out[1] = if ext.authority { EXT_FLAG_AUTHORITY } else { 0 };
    out[4..12].copy_from_slice(&ext.announce.date_offset_ns.to_be_bytes());
    out[12..20].copy_from_slice(&ext.announce.effective_ptp_ns.to_be_bytes());
    out[20..24].copy_from_slice(&ext.announce.seq.to_be_bytes());
    out[24..30].copy_from_slice(&ext.gm_uuid);
    out[32..40].copy_from_slice(&ext.now_ptp_ns.to_be_bytes());
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
        gm_uuid: [
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29],
        ],
        now_ptp_ns: i64_at(32),
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
            current_since_ptp_ns: now_ptp_ns.wrapping_sub(IMMEDIATE_BACKDATE_NS),
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

    /// What the authority publishes: the latest announced offset with its effective PTP instant.
    /// Whether it is still pending is carried by `effective_ptp_ns` against the reader's own
    /// "now" — a step whose instant has passed reads exactly as the promoted in-effect offset
    /// (`current = offset, since = eff`), so no promotion is needed here. Read-only, so
    /// `/status` can publish it from a shared reference.
    pub fn announce(&self) -> DateAnnounce {
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

    /// The size of the pending step (`pending D − D in effect`) while it is still ahead of
    /// `now_ptp_ns`; `None` when nothing is pending.
    pub fn pending_step_ns(&self, now_ptp_ns: i64) -> Option<i64> {
        match self.pending {
            Some((offset, eff)) if eff > now_ptp_ns => Some(offset.wrapping_sub(self.current_ns)),
            _ => None,
        }
    }

    /// `D` in effect without promoting (read-only view of [`current_offset_ns`](Self::current_offset_ns)).
    pub fn in_effect_ns(&self, now_ptp_ns: i64) -> i64 {
        match self.pending {
            Some((offset, eff)) if eff <= now_ptp_ns => offset,
            _ => self.current_ns,
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
        Some(self.announce())
    }

    /// Move `D` WITHOUT a coordinated step, effective immediately: the time base changed (a
    /// grandmaster change or reboot), so the same wall line has a new `D` in the new base.
    ///
    /// `now_ptp_old_ns` is "now" in the time base the authority is in BEFORE this call
    /// (`wall − old D`): it is used to promote a due step first, and then converted into the new
    /// base (`ptp_new = ptp_old − shift`, since `wall = ptp_old + D_old = ptp_new + D_new`).
    ///
    /// A pending step survives a rebase with its WALL instant and its size unchanged: both the
    /// pending offset and its effective PTP instant are shifted into the new base.
    pub fn rebase(&mut self, new_offset_ns: i64, now_ptp_old_ns: i64) -> DateAnnounce {
        self.promote(now_ptp_old_ns);
        let shift = new_offset_ns.wrapping_sub(self.current_ns);
        let now_ptp_new = now_ptp_old_ns.wrapping_sub(shift);
        self.current_ns = new_offset_ns;
        self.current_since_ptp_ns = now_ptp_new.wrapping_sub(IMMEDIATE_BACKDATE_NS);
        if let Some((offset, eff)) = self.pending {
            self.pending = Some((offset.wrapping_add(shift), eff.wrapping_sub(shift)));
        }
        self.over_bound = None;
        self.seq = self.seq.wrapping_add(1);
        self.announce()
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

    /// Drop a scheduled step without touching the alignment (this box already corrected its
    /// wall another way, e.g. a local step).
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    /// Forget the authority alignment (the authority went silent, or this box stopped
    /// following). A step already SCHEDULED is kept: the announce was valid when heard and the rest
    /// of the fleet applies it at its instant, so dropping it would split this box off the fleet.
    pub fn forget(&mut self) {
        self.adopted_seq = None;
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
        // A seq BELOW the adopted one is a new authority session (the master restarted and
        // re-established the offset from seq 1): re-joining it is not a missed announce.
        let first = match self.adopted_seq {
            None => true,
            Some(adopted) => a.seq < adopted,
        };
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
mod tests;
