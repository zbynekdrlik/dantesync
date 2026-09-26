//! The bench's phase measurement (step 6a of each window): the wall disagreement and the relative
//! phase of the fleet at the same true instant, and the wall-ran-back count. In its own file so
//! the harness stays within one screenful per concern.

use super::*;

impl Bench<'_> {
    /// 6a. Raw wall disagreement at the same true instant, INCLUDING each box's own PTP path
    ///     delay (a box holds `t2 − t1 = D`, so its wall sits `delay` behind the GM line) — exactly
    ///     what a cross-box genlock grid sees.
    pub(super) fn measure_disagreement(&mut self, w: u64, t0_ns: f64) {
        if w > self.sc.settle_windows && !self.sc.settling_after_a_utc_jump(w) {
            self.max_fleet_utc = self
                .max_fleet_utc
                .max((self.utc.ns - self.boxes[1].wall_ns()).abs());
        }
        if w <= ALL_JOINED_AFTER {
            return;
        }
        // A coordinated step is IN FLIGHT at this boundary when some box landed it in this window
        // while another still holds it pending (their walls straddle its instant by µs).
        let landed_now = |b: &Box_| -> Vec<u32> {
            b.steps
                .iter()
                .filter(|s| s.2 == StepKind::Coordinated && s.3 >= t0_ns)
                .map(|s| s.0)
                .collect()
        };
        // The master is judged against the fleet only while it is ON the fleet line (PTP online
        // and its D equal to the authority's): during its own PTP outage it runs the local NTP
        // date path by design (micro mode) or free-runs with the fleet D (daily mode, #119 1.12:
        // its free-run error is bounded by the daily scenarios on their own).
        let a = self.authority.as_ref().unwrap();
        let m = &self.boxes[0];
        let m_d = m.d_in_effect();
        let master_on_line =
            !self.sc.master_offline_at(w) && a.in_effect_ns(m.wall_ns() - m_d) == m_d;
        let slewing = a.slew_in_progress(m.wall_ns() - m_d).is_some()
            || self.boxes.iter().any(|b| b.follower.held_slew().is_some());
        if slewing && (w == GM_CHANGE_AT_WINDOW || w == GM_REBOOT_AT_WINDOW) {
            self.gm_events_in_slew += 1;
        }
        let judged: Vec<&Box_> = self
            .boxes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 0 || master_on_line)
            .map(|(_, b)| b)
            .collect();

        // (#119 follow-up: a step a grandmaster rebase re-announced is pending under the rebase's
        // seq on a box that re-anchored first, while the master lands it under the original one.)
        let original_seq = |seq: u32| {
            self.renamed_steps
                .iter()
                .find(|(_, new)| *new == seq)
                .map_or(seq, |(old, _)| *old)
        };
        // Both sides in the ORIGINAL seq: a box may land the step under either.
        let landed: Vec<u32> = judged
            .iter()
            .flat_map(|b| landed_now(b))
            .map(original_seq)
            .collect();
        let straddling = judged.iter().any(|b| {
            b.follower
                .pending()
                .is_some_and(|p| landed.contains(&original_seq(p.seq)))
        });
        // The documented double fault: when the master returns on a new grandmaster it re-bases
        // the fleet D with its own free-run error (~100 µs here). Followers take it at their next
        // poll — a step above the 100 µs absorb tolerance, else an absorb that the phase lock
        // slews out over ~2 minutes. That settling is the fault's cost, not a steady-state
        // disagreement: it is bounded on its own (`max_settling_dis`, and ≤ one late step < 1 ms
        // per follower).
        let sc = self.sc;
        let double_fault_settling = sc.gm_change_in_master_outage
            && sc.master_ptp_offline.iter().any(|&(from, to)| {
                (from..to).contains(&GM_CHANGE_AT_WINDOW) && (to..to + 600).contains(&w)
            });
        let hi = judged.iter().map(|b| b.wall_ns()).max().unwrap();
        let lo = judged.iter().map(|b| b.wall_ns()).min().unwrap();
        if straddling {
            self.in_flight += 1;
        } else if double_fault_settling {
            self.max_settling_dis = self.max_settling_dis.max(hi - lo);
        } else {
            self.max_dis = self.max_dis.max(hi - lo);
            // #119: the relative phase — each wall plus its own path delay (a box holds its wall
            // `delay` behind the GM line, see above).
            let rel: Vec<i64> = judged
                .iter()
                .map(|b| b.wall_ns() + b.delay_ns.round() as i64)
                .collect();
            let spread = rel.iter().max().unwrap() - rel.iter().min().unwrap();
            self.max_rel = self.max_rel.max(spread);
            if slewing {
                self.max_rel_slew = self.max_rel_slew.max(spread);
                self.slew_windows += 1;
            }
        }
        // #119: no wall ever reads less than at the previous boundary.
        for b in self.boxes.iter_mut() {
            let now = b.wall_ns();
            if now < b.last_wall {
                self.wall_back += 1;
            }
            b.last_wall = now;
        }
    }
}
