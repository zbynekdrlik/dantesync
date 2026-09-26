//! dantesync#88 / #119 — a follower acting on the authority's announce: the step it takes now,
//! the step or slew it schedules (a micro-correction scheduled quietly), what it logs.

use super::*;

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    /// Act on (and log) what `DateFollower::on_announce` asked for, for the announce `ann` heard
    /// with this box's anchor `anchor` at `now_wall`; `first` = this box had not adopted yet.
    pub(super) fn act_on_announce(
        &mut self,
        act: FollowAction,
        ann: DateAnnounce,
        anchor: i64,
        now_wall: i64,
        first: bool,
    ) {
        match act {
            FollowAction::None => {}
            FollowAction::Absorb { new_anchor_ns } => {
                self.date_sync.core.set_anchor(new_anchor_ns);
                if first {
                    info!(
                        "[DATE] aligned with the fleet date offset (seq {}): D adopted, {:+}ns \
                         inside the absorb tolerance — no step",
                        ann.seq,
                        new_anchor_ns.wrapping_sub(anchor)
                    );
                }
            }
            // #119 ROZHODNUTÉ: a 1.10+ authority schedules a backward step only beyond the slew
            // cap. A smaller one comes from an older master that never slews (the rollout upgrades
            // the master LAST) — loud too, but not blamed on the cap.
            FollowAction::Scheduled {
                delta_ns,
                effective_wall_ns,
            } if delta_ns < 0 => {
                let cap = crate::date_offset::slew_cap_ns(self.date_sync.step_bound_ns);
                let cause = if delta_ns < -cap {
                    "date correction too large to slew"
                } else {
                    "the authority does not slew (an older dantesync?)"
                };
                warn!(
                    "[DATE] {}: coordinated BACKWARD date step {:+}us scheduled (seq {}) in {} ms",
                    cause,
                    delta_ns / 1_000,
                    ann.seq,
                    effective_wall_ns.wrapping_sub(now_wall) / 1_000_000
                )
            }
            // #119 follow-up: a micro-correction is scheduled quietly (it lands as one info line).
            FollowAction::Scheduled {
                delta_ns,
                effective_wall_ns,
            } if ann.micro => debug!(
                "[DATE] micro date step {:+}us scheduled (seq {}) in {} ms",
                delta_ns / 1_000,
                ann.seq,
                effective_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
            FollowAction::Scheduled {
                delta_ns,
                effective_wall_ns,
            } => info!(
                "[DATE] coordinated date step {:+}us scheduled (seq {}) in {} ms",
                delta_ns / 1_000,
                ann.seq,
                effective_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
            FollowAction::Step { delta_ns, kind } => self.apply_date_step(delta_ns, kind, ann.seq),
            FollowAction::SlewScheduled {
                amount_ns,
                start_wall_ns,
                ppm,
            } if ann.micro => debug!(
                "[DATE] micro date slew {:+}us at {} ppm scheduled (seq {}) in {} ms",
                amount_ns / 1_000,
                ppm,
                ann.seq,
                start_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
            FollowAction::SlewScheduled {
                amount_ns,
                start_wall_ns,
                ppm,
            } => info!(
                "[DATE] coordinated date SLEW {:+}us at {} ppm scheduled (seq {}) in {} ms — no \
                 backward step",
                amount_ns / 1_000,
                ppm,
                ann.seq,
                start_wall_ns.wrapping_sub(now_wall) / 1_000_000
            ),
        }
    }
}
