//! The controller glue the two-clock bench mirrors (`src/controller/date_sync.rs`): kept in these
//! few functions so a change to the controller's glue has exactly one place to be mirrored.

use super::*;

/// The master's published STATUS snapshot (`publish_date_status`): the FLEET D (its authority's
/// announce, even while its own wall is off the fleet line), and its own D in effect. It is only
/// refreshed at the controller's publish points (a PTP window, an NTP cycle, a step, the 10 s
/// tick), and the time server reads it at REPLY time — so a stale snapshot is visible here.
#[derive(Clone, Copy)]
pub(super) struct Published {
    pub(super) date_offset_ns: i64,
    pub(super) effective_ptp_ns: i64,
    pub(super) seq: u32,
    pub(super) slew: Option<SlewSpec>,
    /// #119 follow-up: the published change is a micro-correction.
    pub(super) micro: bool,
    pub(super) gm: u8,
    /// The master's D IN EFFECT at the publish (anchor + its slew's displacement): the time
    /// server derives the replier's PTP now from it.
    pub(super) master_anchor_ns: i64,
}

pub(super) fn master_publishes(m: &Box_, a: &DateAuthority) -> Published {
    let ann = a.announce();
    Published {
        date_offset_ns: ann.date_offset_ns,
        effective_ptp_ns: ann.effective_ptp_ns,
        seq: ann.seq,
        slew: ann.slew,
        micro: ann.micro,
        gm: m.core_gm,
        master_anchor_ns: m.d_in_effect(),
    }
}

/// Whether an NTP cycle refreshes the master's published status (`ntp_under_date_authority`):
/// always — also off line, or an announce would wait for the next 10 s tick.
pub(super) fn master_publishes_after_ntp(_ptp_offline: bool) -> bool {
    true
}

/// Whether the master's local NTP step refreshes it (`note_local_date_step`).
pub(super) const MASTER_PUBLISHES_AFTER_LOCAL_STEP: bool = true;

/// A follower's applicability checks (`service_date_offset`); the time server computes the
/// replier's PTP now from the SNAPSHOT's D in effect and the live wall.
pub(super) fn follower_accepts(b: &Box_, p: &Published, master_wall_now: i64) -> bool {
    if b.core.anchor_ns().is_none() {
        return false;
    }
    let now_ptp = master_wall_now - p.master_anchor_ns;
    !b.core.rebase_pending()
        && b.core_gm == p.gm
        && same_time_base(now_ptp, b.wall_ns(), b.d_in_effect())
}

/// The master's local NTP step while it has no PTP (`note_local_date_step`): its own wall and D
/// only — the fleet D never moves for one box's fault.
pub(super) fn master_local_step(m: &mut Box_, _a: &mut DateAuthority, delta_ns: i64) {
    m.stepped += delta_ns;
    m.core.note_step(delta_ns);
    m.fresh = false;
    m.follower.cancel_pending();
    let anchor = m.core.anchor_ns().expect("anchored");
    m.follower.freeze_slew(anchor, m.wall_ns());
}

/// The master's UTC reading → the authority (`ntp_under_date_authority`): the FLEET line's error
/// (`reading + anchor − fleet`), fed also while the master has no PTP or is off the line, so the
/// fleet stays on UTC. Returns the announce, whether the master schedules it for its own wall
/// (only on the line), and the fleet D it replaces.
pub(super) fn master_feed_authority(
    m: &Box_,
    a: &mut DateAuthority,
    utc_err_ns: i64,
    ptp_offline: bool,
) -> Option<(DateAnnounce, bool, i64)> {
    let d = m.d_in_effect();
    let now_ptp = m.wall_ns() - d;
    let fleet = a.in_effect_ns(now_ptp);
    let fleet_err = utc_err_ns + (d - fleet);
    let on_line = d == fleet && !ptp_offline;
    a.on_utc_error(fleet_err, now_ptp)
        .map(|ann| (ann, on_line, fleet))
}

/// #119 follow-up — the master's micro-correction clock (`tick_date_authority`, every loop
/// iteration; here every window): the authority may announce the next increment. Returns it, whether
/// the master schedules it for its own wall (only on the line), and the fleet D it replaces.
pub(super) fn master_tick_authority(
    m: &Box_,
    a: &mut DateAuthority,
    ptp_offline: bool,
) -> Option<(DateAnnounce, bool, i64)> {
    if m.core.rebase_pending() {
        return None;
    }
    let d = m.d_in_effect();
    let now_ptp = m.wall_ns() - d;
    let fleet = a.in_effect_ns(now_ptp);
    let on_line = d == fleet && !ptp_offline;
    a.on_tick(now_ptp).map(|ann| (ann, on_line, fleet))
}

/// The master re-anchored on a new time base (`handle_phase_anchor_event`): the fleet D moves by
/// the observed base shift, never onto the master's own (possibly off-line) anchor.
pub(super) fn master_rebases_fleet(
    a: &mut DateAuthority,
    master_wall_ns: i64,
    old_ns: i64,
    new_ns: i64,
    displacement_ns: i64,
) {
    // "now" in the OLD base, from the D IN EFFECT (anchor + the slew's displacement).
    let now_ptp_old = master_wall_ns - old_ns - displacement_ns;
    let fleet_old = a.in_effect_ns(now_ptp_old);
    a.rebase(fleet_old + (new_ns - old_ns), now_ptp_old);
}

/// The master's own re-alignment to the fleet line once its PTP is back
/// (`realign_master_to_fleet`): one Join step that lands its wall ON the fleet line (removing the
/// phase error the outage left, measured by a window taken after PTP came back), nothing while a
/// step is pending.
pub(super) fn master_reconcile(m: &mut Box_, a: &DateAuthority, t_ns: f64, w: u64, grace: bool) {
    // (The controller also gates on its step-failure backoff; the bench's clocks never fail.)
    if !m.core.engaged() || m.core.rebase_pending() {
        return;
    }
    let d = m.d_in_effect();
    let now_ptp = m.wall_ns() - d;
    // #119: while the fleet slews the master only catches up with a slew its own scheduler
    // missed, within the absorb tolerance (`catch_up_fleet_slew`); nothing else.
    if let Some(fleet_slew) = a.slew_in_progress(now_ptp) {
        let anchor = m.core.anchor_ns().expect("anchored");
        let gap = a.in_effect_ns(now_ptp) - d;
        if m.follower.held_slew().map(|h| h.slew) != Some(fleet_slew)
            && gap.abs() <= dantesync::date_offset::ABSORB_TOLERANCE_NS
        {
            if let FollowAction::Absorb { new_anchor_ns } =
                m.follower.on_announce(a.announce(), anchor, m.wall_ns())
            {
                m.core.set_anchor(new_anchor_ns);
            }
            m.catch_ups += 1;
        }
        return;
    }
    if a.pending_step_ns(now_ptp).is_some()
        || m.follower.pending().is_some()
        || m.follower.held_slew().is_some()
    {
        return;
    }
    let fleet = a.in_effect_ns(now_ptp);
    if fleet == d || !m.fresh {
        return;
    }
    let e = m.core.last_error_ns().unwrap_or(0);
    let delta = fleet - d - e;
    if delta.abs() > dantesync::date_offset::ABSORB_TOLERANCE_NS {
        m.apply_step(a.seq(), delta, StepKind::Join, t_ns, w, grace);
    }
    m.core.set_anchor(fleet);
}
