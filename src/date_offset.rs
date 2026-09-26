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
//!
//! # dantesync#119 — a correction BACKWARDS is slewed, never stepped
//!
//! A backward step makes wall time run back on every box at once, and wall-time consumers downstream
//! (Dante DVS/ASIO, the camera-box stream OBS) lose that much audio (−43.7 ms at the −51 ms step of
//! camera-box#1372). So only a POSITIVE correction (the fleet is behind UTC) is a coordinated step.
//! A NEGATIVE one is announced as a [`DateSlew`]: from its start instant every box moves `D` down at
//! the same bounded rate (`slew_ppm`, default 100 = 50 ms in 500 s) until the amount is paid. `D`
//! is then a function of PTP time, [`DateSlew::offset_at`], identical on every box, so the relative
//! phase holds; only the fleet-vs-Dante rate deviates by `slew_ppm` while it runs. The wall never
//! runs backwards: the controller applies the slew as an extra rate term of the ONE frequency word
//! and removes the scheduled displacement from every PTP measurement, so neither servo reads it as
//! grandmaster disagreement (`controller/date_sync.rs`).

mod slew;
pub use slew::{
    clamp_slew_ppm, correction_kind, solve_displacement, CorrectionKind, DateSlew, HeldSlew,
    SlewSpec, DEFAULT_SLEW_PPM, MAX_SLEW_PPM, MIN_SLEW_PPM,
};

/// Version of the 31900 reply extension carried by this build (2 = v1 + the #119 slew fields).
pub const EXT_VERSION: u8 = 2;

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
///
/// Version 2 (dantesync#119) keeps those 40 bytes and uses two of them that v1 writes as zero, so a
/// v1 reader simply sees a step (the pre-#119 behaviour):
///
/// ```text
/// [1]      flags: bit 1 = the announce is a SLEW (`date_offset_ns` is where it ends,
///                          `effective_ptp_ns` where it starts)
/// [2-3]    slew_ppm         (u16) — the slew rate, 0 unless bit 1 is set
/// [40-47]  slew_from_ns     (i64) — D where the slew starts
/// ```
pub const EXT_SIZE: usize = 40;

/// Size of the v2 extension (dantesync#119): the v1 fields plus `slew_from_ns`. This build writes
/// it; a reply is only read as a slew when the version is ≥ 2 AND all of it is present.
pub const EXT_SIZE_V2: usize = 48;

/// Extension flag: the replying node is the fleet's date-offset authority.
pub const EXT_FLAG_AUTHORITY: u8 = 0x01;

/// Extension flag (v2, dantesync#119): the announce is a coordinated SLEW, not a step.
pub const EXT_FLAG_SLEW: u8 = 0x02;

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
    /// `D` — wall = PTP time + D. For a slew: the `D` it ends on.
    pub date_offset_ns: i64,
    /// The PTP instant `D` takes effect. In the future = a pending coordinated step. For a slew:
    /// the instant the slew starts.
    pub effective_ptp_ns: i64,
    /// Bumped on every change of `D` (a step, a slew OR a rebase).
    pub seq: u32,
    /// dantesync#119 — `Some` when this change is a coordinated SLEW (a backward correction).
    pub slew: Option<SlewSpec>,
}

impl DateAnnounce {
    /// The slew this announce describes, if it is one. Only a BACKWARD slew (`to < from`) is:
    /// no authority slews forward, and one read off the wire is taken as the plain announce (a
    /// forward step at its instant) — the displacement solve needs the backward direction to
    /// settle on a fixed point.
    pub fn as_slew(&self) -> Option<DateSlew> {
        self.slew
            .filter(|s| self.date_offset_ns < s.from_ns)
            .map(|s| DateSlew {
                from_ns: s.from_ns,
                to_ns: self.date_offset_ns,
                start_ptp_ns: self.effective_ptp_ns,
                ppm: clamp_slew_ppm(s.ppm),
            })
    }
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

/// Encode the v2 extension (see [`EXT_SIZE`] for the layout).
pub fn encode_extension(ext: &DateExtension) -> [u8; EXT_SIZE_V2] {
    let mut out = [0u8; EXT_SIZE_V2];
    out[0] = EXT_VERSION;
    out[1] = if ext.authority { EXT_FLAG_AUTHORITY } else { 0 };
    out[4..12].copy_from_slice(&ext.announce.date_offset_ns.to_be_bytes());
    out[12..20].copy_from_slice(&ext.announce.effective_ptp_ns.to_be_bytes());
    out[20..24].copy_from_slice(&ext.announce.seq.to_be_bytes());
    out[24..30].copy_from_slice(&ext.gm_uuid);
    out[32..40].copy_from_slice(&ext.now_ptp_ns.to_be_bytes());
    if let Some(slew) = ext.announce.slew {
        out[1] |= EXT_FLAG_SLEW;
        let ppm = clamp_slew_ppm(slew.ppm).min(u16::MAX as u32) as u16;
        out[2..4].copy_from_slice(&ppm.to_be_bytes());
        out[40..48].copy_from_slice(&slew.from_ns.to_be_bytes());
    }
    out
}

/// Decode an extension. `None` when it is absent (fewer than [`EXT_SIZE`] bytes — an older
/// server's plain 64-byte reply) or when the version byte is 0 (never a valid extension).
/// Any version ≥ 1 is accepted and read as v1: later versions only append fields. The #119 slew
/// is read only from a version ≥ 2 extension that carries all [`EXT_SIZE_V2`] bytes; its rate is
/// clamped like a configured one.
pub fn decode_extension(bytes: &[u8]) -> Option<DateExtension> {
    if bytes.len() < EXT_SIZE || bytes[0] == 0 {
        return None;
    }
    let i64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[at..at + 8]);
        i64::from_be_bytes(b)
    };
    let slew =
        (bytes[0] >= 2 && bytes.len() >= EXT_SIZE_V2 && bytes[1] & EXT_FLAG_SLEW != 0).then(|| {
            SlewSpec {
                from_ns: i64_at(40),
                ppm: clamp_slew_ppm(u16::from_be_bytes([bytes[2], bytes[3]]) as u32),
            }
        });
    Some(DateExtension {
        version: bytes[0],
        authority: bytes[1] & EXT_FLAG_AUTHORITY != 0,
        announce: DateAnnounce {
            date_offset_ns: i64_at(4),
            effective_ptp_ns: i64_at(12),
            seq: u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            slew,
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
    /// dantesync#119 — the rate of a backward correction's slew.
    slew_ppm: u32,
    /// `D` in effect when no slew runs (a slew moves it along [`DateSlew::offset_at`]).
    current_ns: i64,
    /// The PTP instant `current_ns` took effect.
    current_since_ptp_ns: i64,
    /// A pending coordinated step: (new D, effective PTP instant).
    pending: Option<(i64, i64)>,
    /// dantesync#119 — the coordinated slew announced last, until it is complete (scheduled or
    /// running). While it exists it IS the published announce, and `current_ns` is its `from`.
    slew: Option<DateSlew>,
    seq: u32,
    /// Consecutive same-sign over-bound readings: (sign, count).
    over_bound: Option<(i8, u32)>,
}

impl DateAuthority {
    /// Establish the authority with the master's own PTP-phase-lock anchor as `D`.
    /// `step_bound_ns` ≤ 0 falls back to the default; `lead_ns` is floored at [`MIN_STEP_LEAD_NS`].
    /// Backward corrections slew at [`DEFAULT_SLEW_PPM`] unless [`with_slew_ppm`](Self::with_slew_ppm).
    pub fn new(anchor_ns: i64, now_ptp_ns: i64, step_bound_ns: i64, lead_ns: i64) -> Self {
        DateAuthority {
            step_bound_ns: if step_bound_ns > 0 {
                step_bound_ns
            } else {
                DEFAULT_STEP_BOUND_NS
            },
            lead_ns: lead_ns.max(MIN_STEP_LEAD_NS),
            slew_ppm: DEFAULT_SLEW_PPM,
            current_ns: anchor_ns,
            current_since_ptp_ns: now_ptp_ns.wrapping_sub(IMMEDIATE_BACKDATE_NS),
            pending: None,
            slew: None,
            seq: 1,
            over_bound: None,
        }
    }

    /// dantesync#119 — the slew rate of backward corrections (clamped by [`clamp_slew_ppm`]).
    pub fn with_slew_ppm(mut self, ppm: u32) -> Self {
        self.slew_ppm = clamp_slew_ppm(ppm);
        self
    }

    pub fn slew_ppm(&self) -> u32 {
        self.slew_ppm
    }

    /// dantesync#119 — the announced slew while it is not yet complete at `now_ptp_ns` (scheduled
    /// or running); `None` otherwise.
    pub fn slew_in_progress(&self, now_ptp_ns: i64) -> Option<DateSlew> {
        self.slew.filter(|s| !s.complete_at(now_ptp_ns))
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
        // #119: a slew that has paid its amount leaves its end offset in effect. The seq is kept:
        // D did not change again, so a follower that slewed with it has nothing to do.
        if let Some(s) = self.slew {
            if s.complete_at(now_ptp_ns) {
                self.current_ns = s.to_ns;
                self.current_since_ptp_ns = s.end_ptp_ns();
                self.slew = None;
            }
        }
    }

    /// `D` in effect at `now_ptp_ns`.
    pub fn current_offset_ns(&mut self, now_ptp_ns: i64) -> i64 {
        self.promote(now_ptp_ns);
        self.in_effect_ns(now_ptp_ns)
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
    ///
    /// dantesync#119: while a slew is scheduled or running (and after it, until it is promoted)
    /// the announce IS the slew; read after its end it gives `to` in effect — the same `D` the
    /// promoted form publishes.
    pub fn announce(&self) -> DateAnnounce {
        if let Some(s) = self.slew {
            return s.announce(self.seq);
        }
        match self.pending {
            Some((offset, eff)) => DateAnnounce {
                date_offset_ns: offset,
                effective_ptp_ns: eff,
                seq: self.seq,
                slew: None,
            },
            None => DateAnnounce {
                date_offset_ns: self.current_ns,
                effective_ptp_ns: self.current_since_ptp_ns,
                seq: self.seq,
                slew: None,
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
    /// During a slew it is the slew's schedule at `now_ptp_ns`.
    pub fn in_effect_ns(&self, now_ptp_ns: i64) -> i64 {
        if let Some(s) = self.slew {
            return s.offset_at(now_ptp_ns);
        }
        match self.pending {
            Some((offset, eff)) if eff <= now_ptp_ns => offset,
            _ => self.current_ns,
        }
    }

    /// Feed one UTC measurement: `utc_error_ns = UTC − wall` on the master (the NTP offset).
    ///
    /// Announces a new `D = D + utc_error_ns` when the error has exceeded the bound on
    /// [`AUTHORITY_AGREEMENT_N`] consecutive same-sign readings and no step is already pending.
    /// Returns the new announce when it made one.
    ///
    /// Correct because the master's own wall is `ptp + D`: `UTC − ptp = D + (UTC − wall)`.
    ///
    /// dantesync#119 — the DIRECTION decides how (see [`correction_kind`]): a positive correction
    /// is a coordinated STEP taking effect `lead` from now; a negative one is a coordinated SLEW
    /// at `slew_ppm` starting `lead` from now. The fleet date never steps backwards.
    ///
    /// A slew in progress absorbs a new correction: readings are judged by the error that will
    /// REMAIN once it has paid (`utc_error − (to − D now)`). A further backward need extends the
    /// running slew — re-announced from the current `D` at the same rate, so `D` stays continuous
    /// and a follower that hears it late sees no change — but only while at least one `lead` of
    /// it is left, so a follower hears the extension before the old slew ends. A forward need
    /// waits for the slew to end and is then stepped.
    pub fn on_utc_error(&mut self, utc_error_ns: i64, now_ptp_ns: i64) -> Option<DateAnnounce> {
        self.promote(now_ptp_ns);
        if self.pending.is_some() {
            // One step at a time. Readings taken while a step is pending describe the wall that
            // is about to move; they must not start a second, stale candidate.
            self.over_bound = None;
            return None;
        }
        let running = self.slew;
        let error_ns = match running {
            // The error left once the slew has paid: the wall still moves by `to − D now`.
            Some(s) => utc_error_ns.wrapping_sub(s.to_ns.wrapping_sub(s.offset_at(now_ptp_ns))),
            None => utc_error_ns,
        };
        if error_ns.abs() <= self.step_bound_ns {
            self.over_bound = None;
            return None;
        }
        let sign: i8 = if error_ns > 0 { 1 } else { -1 };
        let count = match self.over_bound {
            Some((s, n)) if s == sign => n + 1,
            _ => 1,
        };
        if count < AUTHORITY_AGREEMENT_N {
            self.over_bound = Some((sign, count));
            return None;
        }
        if let Some(s) = running {
            let extendable = sign < 0
                && s.active_at(now_ptp_ns)
                && s.end_ptp_ns().saturating_sub(now_ptp_ns) >= self.lead_ns;
            if !extendable {
                // Keep the agreement: the correction is made once the slew allows it.
                self.over_bound = Some((sign, count));
                return None;
            }
            self.over_bound = None;
            let from = s.offset_at(now_ptp_ns);
            self.current_ns = from;
            self.current_since_ptp_ns = now_ptp_ns;
            self.slew = Some(DateSlew {
                from_ns: from,
                to_ns: s.to_ns.saturating_add(error_ns),
                start_ptp_ns: now_ptp_ns,
                ppm: s.ppm,
            });
            self.seq = self.seq.wrapping_add(1);
            return Some(self.announce());
        }
        self.over_bound = None;
        let eff = now_ptp_ns.saturating_add(self.lead_ns);
        let target = self.current_ns.saturating_add(error_ns);
        match correction_kind(error_ns) {
            CorrectionKind::Step => self.pending = Some((target, eff)),
            CorrectionKind::Slew => {
                self.slew = Some(DateSlew {
                    from_ns: self.current_ns,
                    to_ns: target,
                    start_ptp_ns: eff,
                    ppm: self.slew_ppm,
                })
            }
        }
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
    ///
    /// dantesync#119: `new_offset_ns` is the `D` IN EFFECT in the new base. A slew in progress is
    /// shifted the same way, so it keeps running at the same wall instants and the same rate.
    pub fn rebase(&mut self, new_offset_ns: i64, now_ptp_old_ns: i64) -> DateAnnounce {
        self.promote(now_ptp_old_ns);
        let shift = new_offset_ns.wrapping_sub(self.in_effect_ns(now_ptp_old_ns));
        let now_ptp_new = now_ptp_old_ns.wrapping_sub(shift);
        match self.slew {
            Some(s) => {
                self.slew = Some(s.shifted(shift));
                self.current_ns = self.current_ns.wrapping_add(shift);
                self.current_since_ptp_ns = self.current_since_ptp_ns.wrapping_sub(shift);
            }
            None => {
                self.current_ns = new_offset_ns;
                self.current_since_ptp_ns = now_ptp_new.wrapping_sub(IMMEDIATE_BACKDATE_NS);
            }
        }
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
    /// Adopt `new_anchor_ns` as the phase lock's anchor without stepping (the new `D` is within
    /// [`ABSORB_TOLERANCE_NS`] of this box's). The anchor is the BASE of `D`: while a slew is held,
    /// `D = anchor + displacement` ([`DateFollower::in_effect_ns`]).
    Absorb { new_anchor_ns: i64 },
    /// Step the wall by `delta_ns` NOW and set `D += delta_ns`.
    Step { delta_ns: i64, kind: StepKind },
    /// A coordinated step was scheduled; [`DateFollower::due`] returns it at the instant.
    Scheduled {
        delta_ns: i64,
        effective_wall_ns: i64,
    },
    /// dantesync#119 — a coordinated slew was scheduled: from `start_wall_ns` this box moves `D`
    /// by `amount_ns` at `ppm`. Nothing jumps now; the controller adds the rate term from the
    /// start ([`DateFollower::slew_rate_ppm`]).
    SlewScheduled {
        amount_ns: i64,
        start_wall_ns: i64,
        ppm: u32,
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
    /// dantesync#119 — the slew this box follows (scheduled, running, or complete but not yet
    /// folded into the anchor).
    held: Option<HeldSlew>,
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

    /// dantesync#119 — the slew this box follows, if any.
    pub fn held_slew(&self) -> Option<HeldSlew> {
        self.held
    }

    /// dantesync#119 — the displacement of `D` from `anchor_ns` at the wall reading `wall_ns`: 0
    /// without a slew. `D` is a function of PTP time and PTP time is `wall − D`, so it is solved
    /// as a fixed point ([`solve_displacement`]): every box and the authority then evaluate the
    /// SAME PTP instant, to the nanosecond. Taken from the WALL (not a sample's `t1`), so it stays
    /// right while a grandmaster change delivers `t1` in another time base.
    pub fn displacement_at_wall(&self, anchor_ns: i64, wall_ns: i64) -> i64 {
        let Some(h) = self.held else {
            return 0;
        };
        solve_displacement(&h, anchor_ns, wall_ns)
    }

    /// dantesync#119 — `D` in effect at `wall_ns` for a box whose phase-lock anchor is `anchor_ns`.
    pub fn in_effect_ns(&self, anchor_ns: i64, wall_ns: i64) -> i64 {
        anchor_ns.wrapping_add(self.displacement_at_wall(anchor_ns, wall_ns))
    }

    /// dantesync#119 — this box's PTP "now" at `wall_ns` (`wall − D in effect`).
    pub fn now_ptp_ns(&self, anchor_ns: i64, wall_ns: i64) -> i64 {
        wall_ns.wrapping_sub(self.in_effect_ns(anchor_ns, wall_ns))
    }

    /// dantesync#119 — the extra frequency term (ppm) the slew asks for at `wall_ns`; 0 outside.
    pub fn slew_rate_ppm(&self, anchor_ns: i64, wall_ns: i64) -> f64 {
        match self.held {
            Some(h) => h.slew.rate_ppm_at(self.now_ptp_ns(anchor_ns, wall_ns)),
            None => 0.0,
        }
    }

    /// dantesync#119 — what the held slew still has to pay at `wall_ns` (`None` without one, or
    /// once it is complete).
    pub fn slew_remaining_ns(&self, anchor_ns: i64, wall_ns: i64) -> Option<i64> {
        let h = self.held?;
        let p = self.now_ptp_ns(anchor_ns, wall_ns);
        (!h.slew.complete_at(p)).then(|| h.slew.remaining_ns(p))
    }

    /// dantesync#119 — once the held slew is complete at `wall_ns`, drop it and return the
    /// displacement to fold into the anchor (`D` is unchanged by the fold).
    pub fn take_completed_slew(&mut self, anchor_ns: i64, wall_ns: i64) -> Option<i64> {
        let h = self.held?;
        if !h.slew.complete_at(self.now_ptp_ns(anchor_ns, wall_ns)) {
            return None;
        }
        self.held = None;
        Some(h.total_displacement())
    }

    /// dantesync#119 — stop following the held slew where it is (`D` stays continuous; the
    /// displacement is folded at the next [`take_completed_slew`](Self::take_completed_slew)).
    /// The NTP master does this when its own local NTP step moves its wall, like
    /// [`cancel_pending`](Self::cancel_pending) for a scheduled step.
    pub fn freeze_slew(&mut self, anchor_ns: i64, wall_ns: i64) {
        let p = self.now_ptp_ns(anchor_ns, wall_ns);
        if let Some(h) = self.held {
            if !h.slew.complete_at(p) {
                self.held = Some(h.frozen_at(p));
            }
        }
    }

    /// dantesync#119 — this box re-anchored onto a new time base whose `D` is `shift_ns` larger:
    /// the held slew moves with it, so it keeps running at the same wall instants.
    pub fn rebase_slew(&mut self, shift_ns: i64) {
        if let Some(h) = self.held.as_mut() {
            h.slew = h.slew.shifted(shift_ns);
            h.ref_ns = h.ref_ns.wrapping_add(shift_ns);
        }
    }

    /// Drop a scheduled step without touching the alignment (this box already corrected its
    /// wall another way, e.g. a local step).
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }

    /// Forget the authority alignment (the authority went silent, or this box stopped
    /// following). A step already SCHEDULED is kept: the announce was valid when heard and the rest
    /// of the fleet applies it at its instant, so dropping it would split this box off the fleet.
    /// A held slew is kept for the same reason.
    pub fn forget(&mut self) {
        self.adopted_seq = None;
    }

    /// Handle an announce from the authority. `own_anchor_ns` is this box's phase-lock anchor —
    /// its `D` without a held slew, the BASE of `D` with one (`D` = anchor + the slew's
    /// displacement, [`in_effect_ns`](Self::in_effect_ns)) — and `now_wall_ns` its wall.
    /// The caller must first run [`due`](Self::due) and
    /// [`take_completed_slew`](Self::take_completed_slew) for the same instant, so a step whose
    /// instant has just passed is applied through the coordinated path, not reported late.
    pub fn on_announce(
        &mut self,
        a: DateAnnounce,
        own_anchor_ns: i64,
        now_wall_ns: i64,
    ) -> FollowAction {
        let own_d = self.in_effect_ns(own_anchor_ns, now_wall_ns);
        let own_now_ptp = now_wall_ns.wrapping_sub(own_d);
        if let Some(slew) = a.as_slew() {
            return self.on_slew_announce(a.seq, slew, own_anchor_ns, own_d, own_now_ptp);
        }
        // #119: the authority's promoted form of the slew this box still runs (`to` in effect at
        // its end, same seq), heard a hair before this box's own end: nothing to do — the slew
        // lands on `to` by itself. (Freezing here would schedule a µs step backwards.)
        if let Some(h) = self.held {
            if self.adopted_seq == Some(a.seq)
                && h.slew.to_ns == a.date_offset_ns
                && h.slew.end_ptp_ns() == a.effective_ptp_ns
            {
                return FollowAction::None;
            }
        }
        // Any other announce without a slew while this box still follows one (a new authority
        // session): stop the slew where it is, so `D` stays continuous, and judge it from there.
        self.freeze_slew(own_anchor_ns, now_wall_ns);
        if a.effective_ptp_ns > own_now_ptp {
            // A pending coordinated step. Only a box already aligned with the authority can
            // schedule it: `delta` is relative to the authority's offset in effect, which this
            // box holds only once it has adopted. An unaligned box waits and joins once it is in
            // effect.
            if self.adopted_seq.is_none() {
                return FollowAction::None;
            }
            let own_at_instant = own_anchor_ns.wrapping_add(
                self.held
                    .map(|h| h.displacement_at_ptp(a.effective_ptp_ns))
                    .unwrap_or(0),
            );
            let step = ScheduledStep {
                seq: a.seq,
                delta_ns: a.date_offset_ns.wrapping_sub(own_at_instant),
                effective_wall_ns: a.effective_ptp_ns.wrapping_add(own_at_instant),
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
        let diff = a.date_offset_ns.wrapping_sub(own_d);
        let kind = self.adopt_in_effect(a.seq);
        self.align(diff, own_anchor_ns, kind)
    }

    /// Record that `seq` is in effect on this box, and say how an out-of-tolerance difference
    /// must be stepped: `Join` for the first adoption (or a re-join of the same seq after a local
    /// wander), `Late` (counted) for an announce this box first heard after its instant.
    fn adopt_in_effect(&mut self, seq: u32) -> StepKind {
        // A seq BELOW the adopted one is a new authority session (the master restarted and
        // re-established the offset from seq 1): re-joining it is not a missed announce.
        let first = match self.adopted_seq {
            None => true,
            Some(adopted) => seq < adopted,
        };
        let already = self.adopted_seq == Some(seq);
        self.adopted_seq = Some(seq);
        // Any scheduled step is superseded by an offset that is already in effect.
        self.pending = None;
        if first || already {
            StepKind::Join
        } else {
            StepKind::Late
        }
    }

    /// The action that brings this box's `D` onto the authority's, `diff_ns` away.
    fn align(&mut self, diff_ns: i64, own_anchor_ns: i64, kind: StepKind) -> FollowAction {
        if diff_ns == 0 {
            return FollowAction::None;
        }
        if diff_ns.abs() <= ABSORB_TOLERANCE_NS {
            return FollowAction::Absorb {
                new_anchor_ns: own_anchor_ns.wrapping_add(diff_ns),
            };
        }
        if kind == StepKind::Late {
            self.late_steps = self.late_steps.saturating_add(1);
        }
        FollowAction::Step {
            delta_ns: diff_ns,
            kind,
        }
    }

    /// dantesync#119 — an announced SLEW. Scheduled ahead of its start (nothing jumps), held while
    /// it runs, and — for a box that first hears it mid-way (a join, or a late poll) — adopted for
    /// its REMAINING part: the box lands on the fleet's current `D` once (the usual join / absorb /
    /// late rules) and slews the rest with everyone else. A re-announce of the slew this box
    /// already holds is idempotent.
    fn on_slew_announce(
        &mut self,
        seq: u32,
        slew: DateSlew,
        own_anchor_ns: i64,
        own_d: i64,
        own_now_ptp: i64,
    ) -> FollowAction {
        let diff = slew.offset_at(own_now_ptp).wrapping_sub(own_d);
        let holds_it = self.held.is_some_and(|h| h.slew == slew);
        if holds_it || slew.complete_at(own_now_ptp) {
            // Already following exactly this slew, or it is over (its `to` is simply in effect;
            // this box folded it, or never needed it). Only a drifted `D` is corrected.
            let kind = self.adopt_in_effect(seq);
            return self.align(diff, own_anchor_ns, kind);
        }
        // Whatever this box held so far stays where it is, as a carried displacement.
        let carry = self
            .held
            .map(|h| h.displacement_at_ptp(own_now_ptp))
            .unwrap_or(0);
        if slew.start_ptp_ns > own_now_ptp {
            // Scheduled. Like a pending step, only a box aligned with the authority can take it.
            if self.adopted_seq.is_none() {
                return FollowAction::None;
            }
            self.held = Some(HeldSlew {
                slew,
                ref_ns: slew.from_ns,
                carry_ns: carry,
            });
            self.adopted_seq = Some(seq);
            // One change of D at a time: a scheduled step is superseded (the authority never has
            // both).
            self.pending = None;
            return FollowAction::SlewScheduled {
                amount_ns: slew.amount_ns(),
                start_wall_ns: slew.start_ptp_ns.wrapping_add(own_d),
                ppm: slew.ppm,
            };
        }
        // Running: follow its remaining part from the fleet's current D.
        self.held = Some(HeldSlew {
            slew,
            ref_ns: slew.offset_at(own_now_ptp),
            carry_ns: carry,
        });
        let kind = self.adopt_in_effect(seq);
        self.align(diff, own_anchor_ns, kind)
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
