//! The bench box's own operations: its wall, its D in effect, its slew and its steps (the
//! controller's `apply_date_step`).

use super::*;

impl Box_ {
    pub(super) fn wall_ns(&self) -> i64 {
        self.wall.ns + self.stepped + self.slewed_ns
    }

    /// #119: `D` in effect (the anchor plus the held slew's displacement at the wall).
    pub(super) fn d_in_effect(&self) -> i64 {
        let anchor = self.core.anchor_ns().expect("anchored");
        self.follower.in_effect_ns(anchor, self.wall_ns())
    }

    /// #119: the held slew's displacement at the current wall (0 before the first anchor).
    pub(super) fn slew_displacement(&self) -> i64 {
        match self.core.anchor_ns() {
            Some(anchor) => self.follower.displacement_at_wall(anchor, self.wall_ns()),
            None => 0,
        }
    }

    /// #119: integrate the slew's rate term over one window, switching it at the PTP instants
    /// where the slew starts and ends (PTP time advances at the true rate to ≪ 1 ns per window:
    /// the grandmasters run at 0 and +3 ppm).
    pub(super) fn advance_slew(&mut self, true_dt_ns: f64) {
        let (Some(anchor), Some(h)) = (self.core.anchor_ns(), self.follower.held_slew()) else {
            return;
        };
        let p0 = self.follower.now_ptp_ns(anchor, self.wall_ns()) as f64;
        let p1 = p0 + true_dt_ns;
        let on = (h.slew.start_ptp_ns as f64).max(p0);
        let off = (h.slew.end_ptp_ns() as f64).min(p1);
        if off > on {
            let rate_ppm = h.slew.amount_ns().signum() as f64 * h.slew.ppm as f64;
            let d = rate_ppm * 1e-6 * (off - on) + self.slew_frac;
            let whole = d.floor();
            self.slew_frac = d - whole;
            self.slewed_ns += whole as i64;
        }
    }

    /// Step the wall and move D with it (the controller's `apply_date_step`), recording it; with
    /// `grace`, the next 2 s of PTP windows are dropped as the controller does after any step.
    pub(super) fn apply_step(
        &mut self,
        seq: u32,
        delta_ns: i64,
        kind: StepKind,
        t_ns: f64,
        w: u64,
        grace: bool,
    ) {
        // #119 (1.11.1): a Windows box's wall moves by what the step law realizes against the
        // Windows clock model; D moves by the REQUESTED amount, as `apply_date_step` does.
        let realized = match self.win.as_mut() {
            Some(win) => win.step(t_ns, delta_ns),
            None => delta_ns,
        };
        self.stepped += realized;
        self.last_landing = Some((t_ns, realized));
        self.core.note_step(delta_ns);
        self.fresh = false;
        self.steps.push((seq, delta_ns, kind, t_ns));
        if grace {
            self.grace_until = w + 1 + GRACE_WINDOWS;
        }
    }
}
