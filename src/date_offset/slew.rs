//! dantesync#119 — the coordinated date SLEW: how a BACKWARD fleet date correction is applied.
//!
//! A backward step makes wall time run back on every box at once, and wall-time consumers
//! downstream (Dante DVS/ASIO, the camera-box stream OBS) lose that much audio. So the direction
//! decides ([`correction_kind`]): forward = the coordinated step, backward = a [`DateSlew`] every
//! box follows. Pure (explicit time inputs, no I/O), like the rest of `crate::date_offset`.

use super::DateAnnounce;

/// dantesync#119 — the default slew rate of a backward correction: 100 ppm pays 50 ms in 500 s.
/// Every box moves by this rate against the Dante tick while a slew runs; camera-box's ASRC takes
/// it as a rate (it tracks hundreds of ppm) and 100 ppm of pitch is inaudible.
pub const DEFAULT_SLEW_PPM: u32 = 100;

/// dantesync#119 — the configurable slew rate is clamped to this range. Below 10 ppm a 50 ms
/// correction takes over 80 minutes and a fast grandmaster-vs-UTC drift could outrun it; above
/// 500 ppm the slew leaves the ±500 ppm frequency envelope of the servos.
pub const MIN_SLEW_PPM: u32 = 10;
pub const MAX_SLEW_PPM: u32 = 500;

/// dantesync#119 — the slew rate actually used for a configured (or received) value: `0` means
/// the default, anything else is clamped to [`MIN_SLEW_PPM`]..=[`MAX_SLEW_PPM`].
pub fn clamp_slew_ppm(ppm: u32) -> u32 {
    if ppm == 0 {
        DEFAULT_SLEW_PPM
    } else {
        ppm.clamp(MIN_SLEW_PPM, MAX_SLEW_PPM)
    }
}

/// dantesync#119 — how a fleet date correction is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorrectionKind {
    /// Every box steps its wall at the announced instant (forward only: the fleet is behind UTC).
    Step,
    /// Every box moves `D` down at `slew_ppm` from the announced instant: the wall never runs back.
    Slew,
}

/// dantesync#119 — THE direction decision. `correction_ns` is the change of `D` (`UTC − wall`):
/// positive moves the wall forward, which every consumer tolerates (a forward step lost no audio
/// on the rig); negative would move it BACKWARDS, which Dante DVS/ASIO turns into lost samples.
pub fn correction_kind(correction_ns: i64) -> CorrectionKind {
    if correction_ns >= 0 {
        CorrectionKind::Step
    } else {
        CorrectionKind::Slew
    }
}

/// dantesync#119 — the slew part of an announce (the rest is in [`DateAnnounce`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlewSpec {
    /// `D` where the slew starts.
    pub from_ns: i64,
    /// The slew rate (ppm of PTP time).
    pub ppm: u32,
}

/// dantesync#119 — a coordinated date SLEW: `D` moves from `from_ns` to `to_ns` at `ppm`, starting
/// at the PTP instant `start_ptp_ns`. The whole schedule is a pure function of PTP time, so every
/// box that holds the same slew has the same `D` at the same PTP instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateSlew {
    pub from_ns: i64,
    pub to_ns: i64,
    pub start_ptp_ns: i64,
    /// Always within [`MIN_SLEW_PPM`]..=[`MAX_SLEW_PPM`] (built through [`clamp_slew_ppm`]).
    pub ppm: u32,
}

impl DateSlew {
    /// `to − from` (negative for the backward corrections this exists for).
    pub fn amount_ns(&self) -> i64 {
        self.to_ns.wrapping_sub(self.from_ns)
    }

    fn rate(&self) -> i128 {
        clamp_slew_ppm(self.ppm) as i128
    }

    /// How long the slew runs (PTP ns): `ceil(|amount| / ppm)`, so [`offset_at`](Self::offset_at)
    /// reaches `to` exactly at [`end_ptp_ns`](Self::end_ptp_ns) and not a nanosecond earlier.
    pub fn duration_ns(&self) -> i64 {
        let amount = self.amount_ns().unsigned_abs() as u128;
        let rate = clamp_slew_ppm(self.ppm) as u128;
        // A ceiling division by hand: `div_ceil` is newer than the crate's MSRV (1.70).
        ((amount * 1_000_000 + rate - 1) / rate).min(i64::MAX as u128) as i64
    }

    pub fn end_ptp_ns(&self) -> i64 {
        self.start_ptp_ns.saturating_add(self.duration_ns())
    }

    /// `D` at the PTP instant `ptp_ns`: `from` up to the start, then `ppm` ns per ms of PTP time
    /// (floored to the ns) towards `to`, then `to`.
    pub fn offset_at(&self, ptp_ns: i64) -> i64 {
        let elapsed = (ptp_ns as i128) - (self.start_ptp_ns as i128);
        if elapsed <= 0 {
            return self.from_ns;
        }
        let amount = self.amount_ns() as i128;
        let paid = (elapsed * self.rate() / 1_000_000).min(amount.abs());
        (self.from_ns as i128 + amount.signum() * paid) as i64
    }

    /// True while `D` is moving at `ptp_ns` (from the start, up to but excluding the end).
    pub fn active_at(&self, ptp_ns: i64) -> bool {
        self.amount_ns() != 0 && ptp_ns >= self.start_ptp_ns && ptp_ns < self.end_ptp_ns()
    }

    /// True once the slew has paid its whole amount.
    pub fn complete_at(&self, ptp_ns: i64) -> bool {
        ptp_ns >= self.end_ptp_ns()
    }

    /// The rate of `D` (ppm, signed: negative for a backward slew) at `ptp_ns`; 0 outside it.
    /// This is the extra frequency term every box adds to its word while the slew runs.
    pub fn rate_ppm_at(&self, ptp_ns: i64) -> f64 {
        if self.active_at(ptp_ns) {
            self.amount_ns().signum() as f64 * clamp_slew_ppm(self.ppm) as f64
        } else {
            0.0
        }
    }

    /// What is still to be paid at `ptp_ns` (ns, ≥ 0).
    pub fn remaining_ns(&self, ptp_ns: i64) -> i64 {
        self.to_ns.wrapping_sub(self.offset_at(ptp_ns)).abs()
    }

    /// The same slew in a time base whose `D` is `shift` larger (a grandmaster change / rebase:
    /// `ptp_new = ptp_old − shift`), so it runs at the same wall instants.
    pub fn shifted(&self, shift_ns: i64) -> Self {
        DateSlew {
            from_ns: self.from_ns.wrapping_add(shift_ns),
            to_ns: self.to_ns.wrapping_add(shift_ns),
            start_ptp_ns: self.start_ptp_ns.wrapping_sub(shift_ns),
            ppm: self.ppm,
        }
    }

    pub(super) fn announce(&self, seq: u32) -> DateAnnounce {
        DateAnnounce {
            date_offset_ns: self.to_ns,
            effective_ptp_ns: self.start_ptp_ns,
            seq,
            slew: Some(SlewSpec {
                from_ns: self.from_ns,
                ppm: self.ppm,
            }),
        }
    }
}

/// dantesync#119 — the slew a box follows. Its `D` is `anchor + displacement(ptp)`, with
/// `displacement = carry + slew.offset_at(ptp) − ref`: the anchor stays the phase lock's base and
/// the slew moves `D` on top of it until the box folds the paid amount into the anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldSlew {
    pub slew: DateSlew,
    /// The slew offset the displacement counts from: its `from` when adopted before the start,
    /// its offset at the adoption instant when joined mid-way (the "remaining part").
    pub ref_ns: i64,
    /// Displacement already accrued by a slew this one replaced (kept, folded together).
    pub carry_ns: i64,
}

impl HeldSlew {
    /// The displacement of `D` from the anchor at the PTP instant `ptp_ns`.
    pub fn displacement_at_ptp(&self, ptp_ns: i64) -> i64 {
        self.carry_ns
            .wrapping_add(self.slew.offset_at(ptp_ns))
            .wrapping_sub(self.ref_ns)
    }

    /// The displacement once the slew is complete — what is folded into the anchor.
    pub fn total_displacement(&self) -> i64 {
        self.carry_ns
            .wrapping_add(self.slew.to_ns)
            .wrapping_sub(self.ref_ns)
    }

    /// The same displacement, frozen where it is at `ptp_ns`: a zero-length slew, complete at
    /// once, that carries it (the next [`DateFollower::take_completed_slew`] folds it).
    pub(super) fn frozen_at(&self, ptp_ns: i64) -> Self {
        HeldSlew {
            slew: DateSlew {
                from_ns: 0,
                to_ns: 0,
                start_ptp_ns: ptp_ns,
                ppm: self.slew.ppm,
            },
            ref_ns: 0,
            carry_ns: self.displacement_at_ptp(ptp_ns),
        }
    }
}

/// dantesync#119 — the displacement of `D` from `anchor_ns` at the wall reading `wall_ns` for a box
/// holding `h`: the fixed point `d = displacement(wall − anchor − d)`.
///
/// Iterated until it stops changing. For a backward slew the map is non-decreasing in `d` with a
/// slope ≤ 500 ppm, so the iterates are monotone and settle on an exact fixed point within a few
/// rounds; the cap only bounds a pathological input. At a nanosecond boundary of the floored
/// schedule two adjacent PTP instants can give the same wall; the solve then settles on one of
/// them, and since every box and the authority evaluate that SAME instant, their `D` agree to
/// the nanosecond.
pub fn solve_displacement(h: &HeldSlew, anchor_ns: i64, wall_ns: i64) -> i64 {
    let base = wall_ns.wrapping_sub(anchor_ns);
    let mut d = h.carry_ns;
    for _ in 0..SOLVE_MAX_ROUNDS {
        let next = h.displacement_at_ptp(base.wrapping_sub(d));
        if next == d {
            break;
        }
        d = next;
    }
    d
}

/// A bound on the fixed-point rounds of [`solve_displacement`] (it settles in ≤ 4 at 500 ppm).
const SOLVE_MAX_ROUNDS: usize = 16;

#[cfg(test)]
mod tests;
