//! dantesync#126 — the NTP master's fleet date offset survives a restart.
//!
//! A restart of the master used to change the fleet date by itself: the new process stepped its
//! own wall to UTC at boot, anchored a fresh `D` on that stepped wall and started a new authority
//! session at seq 1, while every follower dropped the silent authority after 30 s and stepped on
//! its OWN next NTP samples — the whole rig stepped at scattered instants 30–45 s apart (dev1 on
//! 30.9.2026: +247.77 ms at 06:02:42Z).
//!
//! Now the master saves its authority's published state whenever it changes, and a restarted
//! master with a valid saved state restores it: the same `D`, the same seq, the change in flight,
//! the last nightly step. The accumulated UTC error is left to the next nightly window (or the
//! emergency cap), exactly as if the process had never stopped.
//!
//! This module is the PURE part: the record ([`DateOffsetState`]), the authority's part of it
//! ([`AuthorityState`], taken by [`super::DateAuthority::persisted`] and restored by
//! [`super::DateAuthority::restore`]), and the restore decision ([`DateOffsetState::validate_restore`]).
//! The JSON file (`date-offset.json` beside `config.json`, written by temp + rename) is the
//! controller's (`controller/date_sync/persist.rs`), so this stays serde-free and the standalone
//! `rustc` replica keeps covering it.

use super::DateSlew;

/// The version of the saved record. A record of another version is not restored (the boot step
/// and a fresh authority then run, as before 1.15).
pub const STATE_VERSION: u32 = 1;

/// The date authority's published state — what it saves and restores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorityState {
    /// `D` in effect (ns), with every change due by the snapshot instant landed.
    pub d_ns: i64,
    /// The PTP instant `d_ns` took effect (the announce's instant while nothing is in flight).
    pub since_ptp_ns: i64,
    /// The authority session's seq: a restored master keeps it, so followers see the SAME session
    /// (a seq below their adopted one would read as a new session and make them re-join).
    pub seq: u32,
    /// A coordinated step announced and not landed yet: (the new `D`, its PTP instant). Saved so a
    /// restart inside a step's lead neither drops nor repeats a step the fleet already scheduled.
    pub pending: Option<(i64, i64)>,
    /// A coordinated slew announced and not complete (dantesync#119, micro mode).
    pub slew: Option<DateSlew>,
    /// The change behind `seq` was a micro-correction (published with it).
    pub micro: bool,
    /// The last nightly step: (the fleet-wall instant it lands on, its size), ns. Restored, so a
    /// restart neither decides the same night twice nor loses `date_daily_last_step_*`.
    pub daily_last_step: Option<(i64, i64)>,
}

impl AuthorityState {
    /// `D` this state has in effect at `now_ptp_ns`: a pending step whose instant has come has
    /// landed, a slew is at its schedule.
    pub fn d_in_effect_at(&self, _now_ptp_ns: i64) -> i64 {
        self.d_ns // RED stub (#126): the change in flight is not applied yet
    }
}

/// The saved record: the authority's state, the grandmaster whose PTP time base `D` belongs to,
/// and when it was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateOffsetState {
    pub authority: AuthorityState,
    /// The grandmaster of the anchor `D` belongs to (a restore under another one is refused).
    pub gm_uuid: [u8; 6],
    /// The master's wall when the record was written (ns since the epoch).
    pub written_wall_ns: i64,
    /// Its PTP time then (`wall − D in effect`).
    pub written_ptp_ns: i64,
}

/// Why a saved state is not restored. The master then runs the path it ran before 1.15: the boot
/// step to UTC, a fresh anchor and a new authority session at seq 1 — logged loudly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestoreRejected {
    /// No grandmaster identity is known at the first lock.
    NoGrandmaster,
    /// The saved `D` belongs to another grandmaster's time base.
    OtherGrandmaster { saved: [u8; 6], now: [u8; 6] },
    /// The first locked window reads the wall `off_ns` off the saved `D` (`anchor − saved D`),
    /// beyond `cap_ns`: the grandmaster restarted its uptime under the same identity (days off),
    /// or the wall is too far off the fleet line to be re-joined by the master alone.
    TimeBase { off_ns: i64, cap_ns: i64 },
}

impl DateOffsetState {
    /// May the saved state be restored by a master whose phase lock anchored `anchor_ns` (the
    /// median `t2 − t1` of its first locked window) under the grandmaster `gm`? Yes when it is the
    /// same grandmaster and the anchor is within `cap_ns` (the daily emergency cap) of the saved
    /// `D` in effect at that instant. Returns how far the master's wall is off the restored line
    /// (`anchor − saved D`): the master re-aligns its OWN wall by that much, never the fleet.
    pub fn validate_restore(
        &self,
        gm: Option<[u8; 6]>,
        anchor_ns: i64,
        now_wall_ns: i64,
        cap_ns: i64,
    ) -> Result<i64, RestoreRejected> {
        let _ = (gm, anchor_ns, now_wall_ns, cap_ns);
        Ok(0) // RED stub (#126): nothing is validated yet
    }
}

#[cfg(test)]
mod tests;
