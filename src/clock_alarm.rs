//! Loud, repeating "NO DANTE CLOCK" alarm decision (dantesync#114).
//!
//! The loss of the Dante PTP clock used to be SILENT and non-repeating: when the
//! grandmaster stopped matching the allowlist, the controller logged the NTP-only
//! fallback exactly once and went quiet, and the tray only toasted on state
//! transitions. A whole-fleet fall to NTP-only ran for hours unnoticed until a
//! release E2E gate failed (the incident that motivated this module).
//!
//! This is the pure, unit-testable decision layer for a loud, REPEATING alarm.
//! While the node is genuinely PTP-locked to an allowed grandmaster it is
//! completely silent; while the Dante clock is lost it asks for exactly one
//! notification per cadence (default 60 s) until re-lock, and reports LOST /
//! REGAINED edge events so the incident window is reconstructible. The alarm
//! itself is ALWAYS ON — the only knob is the cadence (the rig's
//! features-default-on rule).
//!
//! Emission is split by platform, driven from the single [`drive`] seam:
//! - a WARN log line every cadence on EVERY platform (headless camboxes rely on
//!   this line, which the dev1 watchdog relays);
//! - a Linux desktop `notify-send -u critical` (imag), no-op on a headless box;
//! - Windows: the session-0 service cannot raise a user-session toast, so the
//!   TRAY shows the balloon from the `/status.clock_alarm` field it already
//!   receives over the named pipe — see `src/bin/tray.rs`.
//!
//! The timekeeping discipline of `clock-discipline-and-testing.md` applies:
//! cadence timing uses monotonic [`Instant`] (the daemon steps its own wall
//! clock), while the displayed `since` is a wall-clock epoch published in
//! `/status`.

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// The floor for the alarm cadence — the config knob cannot make it faster than
/// this (spam guard), mirroring the `ntp_stale_secs` flooring discipline in
/// `config-migration.md`.
pub const CLOCK_ALARM_INTERVAL_FLOOR_S: u64 = 10;

/// Health of the Dante PTP clock, sampled by the controller each tick.
#[derive(Debug, Clone)]
pub struct ClockHealth {
    /// The frequency servo is locked (`SyncStatus.is_locked`).
    pub is_locked: bool,
    /// The published mode is a genuinely PTP-locked mode (`LOCK` or `NANO`).
    pub mode_locked: bool,
    /// A grandmaster source IP is present AND permitted by the (resolved)
    /// allowlist.
    pub gm_allowed: bool,
    /// No allowed PTP packet has been seen within the offline timeout.
    pub ptp_stale: bool,
    /// dantesync#113: a configured grandmaster hostname could not be resolved.
    /// `Some(name)` names the first unresolvable hostname; drives the alarm ACTIVE
    /// with a specific reason even before any PTP packet is seen.
    pub allowlist_unresolvable: Option<String>,
    /// dantesync#114 review: the node is still in its INITIAL acquisition window
    /// (never locked yet, within a grace period from start) AND packets are
    /// arriving. A not-yet-locked clock during normal boot acquisition is NOT a
    /// loss — this suppresses the "not-locked" reason so a service restart / rig
    /// reboot does not emit a spurious critical alarm. A genuine HARD failure
    /// (PTP stale = no packets at all, or an unresolvable hostname) still fires
    /// immediately, even during the grace, because those are real clock loss.
    pub in_acquisition: bool,
}

impl ClockHealth {
    /// The single most-actionable reason the Dante clock is considered LOST, or
    /// `None` when the node is genuinely PTP-locked to an allowed grandmaster.
    ///
    /// Precedence, most specific / actionable first: an unresolvable allowlist
    /// hostname (#113), then PTP staleness (no allowed announce), then a plain
    /// not-locked / acquiring state, then a present-but-disallowed source.
    ///
    /// Note on ordering: the allowlist DROPS disallowed packets before adoption
    /// (`process_loop_iteration`), so `gm_allowed` is really "an allowed source is
    /// adopted"; when it is false there is simply no source yet (cold start /
    /// loss), which is best reported as "not PTP-locked". The `gm_allowed`
    /// catch-all below therefore only fires in the near-impossible locked-but-no-
    /// allowed-source case, and is kept for completeness.
    pub fn lost_reason(&self) -> Option<String> {
        if let Some(name) = &self.allowlist_unresolvable {
            return Some(format!("grandmaster hostname {name} unresolvable"));
        }
        // Genuinely healthy: locked, in a locked mode, on an allowed GM, not stale.
        if self.is_locked && self.mode_locked && self.gm_allowed && !self.ptp_stale {
            return None;
        }
        if self.ptp_stale {
            return Some("no PTP announce from an allowed grandmaster".to_string());
        }
        // Initial acquisition (never locked yet, packets flowing, within the grace
        // window): a not-yet-locked clock is normal boot behaviour, not a loss.
        // Hard failures above (stale / unresolvable) already returned.
        if self.in_acquisition {
            return None;
        }
        if !self.is_locked || !self.mode_locked {
            return Some("not PTP-locked to the grandmaster".to_string());
        }
        Some("PTP source not in the grandmaster allowlist".to_string())
    }
}

/// A serializable snapshot of the alarm for `/status` (dantesync#114) — the
/// cross-repo contract external gates read. Additive; `active=false` / `since=None`
/// / `reason=""` is the healthy default so an old JSON blob deserializes cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClockAlarmStatus {
    /// True while the Dante clock is considered lost.
    pub active: bool,
    /// Unix epoch second the current alarm episode began; `None` when inactive
    /// (an explicit `null`, never a misleading `0`).
    pub since: Option<u64>,
    /// Human-readable reason; empty string when inactive.
    pub reason: String,
}

/// What one [`ClockAlarm::evaluate`] produced — the emissions the caller performs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlarmTick {
    /// Emit the loud per-cadence notification NOW (WARN log + platform notifier).
    pub emit_notification: bool,
    /// The alarm just transitioned into the active state (log LOST at INFO).
    pub log_lost: bool,
    /// The alarm just cleared (log REGAINED at INFO).
    pub log_regained: bool,
    /// The snapshot to publish in `/status`.
    pub snapshot: ClockAlarmStatus,
}

/// The per-node alarm state machine. Silent while healthy; while the clock is
/// lost it asks for exactly one notification per `interval` until re-lock.
#[derive(Debug)]
pub struct ClockAlarm {
    interval: Duration,
    active: bool,
    since_epoch: Option<u64>,
    last_notified: Option<Instant>,
    reason: String,
}

impl ClockAlarm {
    /// Build with an explicit cadence (already floored by the caller if it came
    /// from config — see [`from_interval_secs`](Self::from_interval_secs)).
    pub fn new(interval: Duration) -> Self {
        ClockAlarm {
            interval,
            active: false,
            since_epoch: None,
            last_notified: None,
            reason: String::new(),
        }
    }

    /// Build from a config cadence (seconds), flooring nonsense values so a
    /// `clock_alarm_interval_s: 0` can never turn the alarm into a per-tick spam.
    pub fn from_interval_secs(interval_s: u64) -> Self {
        Self::new(Duration::from_secs(
            interval_s.max(CLOCK_ALARM_INTERVAL_FLOOR_S),
        ))
    }

    /// True while the Dante clock is considered lost.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Evaluate the current health and advance the state machine.
    ///
    /// - Healthy → completely silent; snapshot inactive. A transition from active
    ///   sets `log_regained`.
    /// - Lost → `active`, `since` pinned to the epoch of the FIRST lost tick and
    ///   held for the whole episode; a transition into active sets `log_lost` and
    ///   forces an immediate first notification; thereafter exactly one
    ///   notification per `interval`.
    pub fn evaluate(&mut self, health: &ClockHealth, now: Instant, now_epoch: u64) -> AlarmTick {
        match health.lost_reason() {
            Some(reason) => {
                let mut log_lost = false;
                if !self.active {
                    self.active = true;
                    self.since_epoch = Some(now_epoch);
                    self.last_notified = None; // force an immediate first notification
                    log_lost = true;
                }
                // The reason may change while the alarm stays active (e.g. a stale
                // clock whose hostname then becomes unresolvable) — keep it fresh.
                self.reason = reason;
                let emit = match self.last_notified {
                    None => true,
                    Some(t) => now.saturating_duration_since(t) >= self.interval,
                };
                if emit {
                    self.last_notified = Some(now);
                }
                AlarmTick {
                    emit_notification: emit,
                    log_lost,
                    log_regained: false,
                    snapshot: ClockAlarmStatus {
                        active: true,
                        since: self.since_epoch,
                        reason: self.reason.clone(),
                    },
                }
            }
            None => {
                let log_regained = self.active;
                self.active = false;
                self.since_epoch = None;
                self.last_notified = None;
                self.reason.clear();
                AlarmTick {
                    emit_notification: false,
                    log_lost: false,
                    log_regained,
                    snapshot: ClockAlarmStatus::default(),
                }
            }
        }
    }
}

/// Platform sink for the loud desktop notification (dantesync#114).
///
/// The WARN log line is emitted by [`drive`] on EVERY platform regardless of this
/// trait; this is the ADDITIONAL desktop popup. Behind a trait so tests inject a
/// counting fake and never touch a real desktop bus.
pub trait ClockAlarmNotifier {
    fn notify(&self, title: &str, message: &str);
}

/// The real desktop notifier for the running platform.
pub struct DesktopNotifier;

impl ClockAlarmNotifier for DesktopNotifier {
    #[cfg(target_os = "linux")]
    fn notify(&self, title: &str, message: &str) {
        // Only attempt on a box that actually has a desktop bus (imag). Headless
        // camboxes have no DISPLAY/DBUS and skip silently — their signal is the
        // WARN log line, which the dev1 watchdog relays.
        let has_desktop = std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        if !has_desktop {
            return;
        }
        let _ = std::process::Command::new("notify-send")
            .arg("-u")
            .arg("critical")
            .arg(title)
            .arg(message)
            .spawn();
    }

    #[cfg(not(target_os = "linux"))]
    fn notify(&self, _title: &str, _message: &str) {
        // Windows: the session-0 service cannot raise a user-session toast; the
        // tray shows the balloon from `/status.clock_alarm`. Other non-Linux: no-op.
    }
}

/// The title used for every clock-alarm surface.
pub const CLOCK_ALARM_TITLE: &str = "DanteSync — NO DANTE CLOCK";

/// Compose the operator-facing message. `since_epoch` is rendered to local HH:MM
/// (via the crate's existing `chrono` dependency); an absent/unformattable epoch
/// degrades to "just now" rather than printing a raw number.
pub fn compose_message(reason: &str, since_epoch: Option<u64>) -> String {
    let since_str = since_epoch
        .and_then(format_hhmm_local)
        .unwrap_or_else(|| "just now".to_string());
    format!("NO DANTE CLOCK — {reason}, running on NTP fallback since {since_str}")
}

fn format_hhmm_local(epoch: u64) -> Option<String> {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch as i64, 0) {
        chrono::LocalResult::Single(dt) => Some(dt.format("%H:%M").to_string()),
        _ => None,
    }
}

/// The SINGLE emission seam: advance the alarm, then perform the side effects the
/// tick asks for — the LOST/REGAINED INFO edges, the per-cadence WARN log (every
/// platform), and the per-cadence desktop notification through the injected
/// notifier. Centralised here so a test can drive a whole sequence through one
/// function with a fake notifier and assert the cadence.
pub fn drive<N: ClockAlarmNotifier + ?Sized>(
    alarm: &mut ClockAlarm,
    health: &ClockHealth,
    now: Instant,
    now_epoch: u64,
    notifier: &N,
) -> AlarmTick {
    let tick = alarm.evaluate(health, now, now_epoch);
    if tick.log_lost {
        log::info!(
            "[CLOCK-ALARM] LOST — {} (Dante clock lost; running on NTP fallback)",
            tick.snapshot.reason
        );
    }
    if tick.log_regained {
        log::info!("[CLOCK-ALARM] REGAINED — Dante clock re-locked");
    }
    if tick.emit_notification {
        let msg = compose_message(&tick.snapshot.reason, tick.snapshot.since);
        log::warn!("[CLOCK-ALARM] {msg}");
        notifier.notify(CLOCK_ALARM_TITLE, &msg);
    }
    tick
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn healthy() -> ClockHealth {
        ClockHealth {
            is_locked: true,
            mode_locked: true,
            gm_allowed: true,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: false,
        }
    }

    fn lost_not_locked() -> ClockHealth {
        ClockHealth {
            is_locked: false,
            mode_locked: false,
            gm_allowed: true,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: false,
        }
    }

    #[test]
    fn healthy_is_completely_silent() {
        let mut alarm = ClockAlarm::from_interval_secs(60);
        let t0 = Instant::now();
        let tick = alarm.evaluate(&healthy(), t0, 1000);
        assert!(!tick.emit_notification);
        assert!(!tick.log_lost);
        assert!(!tick.log_regained);
        assert!(!tick.snapshot.active);
        assert_eq!(tick.snapshot.since, None);
        assert_eq!(tick.snapshot.reason, "");
        assert!(!alarm.is_active());

        // Still silent on a repeat tick.
        let tick2 = alarm.evaluate(&healthy(), t0 + Duration::from_secs(120), 1120);
        assert!(!tick2.emit_notification);
        assert!(!tick2.log_regained);
    }

    #[test]
    fn unlocked_emits_exactly_once_per_cadence() {
        let mut alarm = ClockAlarm::from_interval_secs(60);
        let t0 = Instant::now();

        // First lost tick: immediate notification + LOST edge, since pinned.
        let a = alarm.evaluate(&lost_not_locked(), t0, 5000);
        assert!(
            a.emit_notification,
            "first lost tick must notify immediately"
        );
        assert!(a.log_lost);
        assert!(a.snapshot.active);
        assert_eq!(a.snapshot.since, Some(5000));

        // 30 s later (still inside the cadence): silent.
        let b = alarm.evaluate(&lost_not_locked(), t0 + Duration::from_secs(30), 5030);
        assert!(
            !b.emit_notification,
            "must not re-notify within the cadence"
        );
        assert!(!b.log_lost, "no repeated LOST edge");
        assert_eq!(
            b.snapshot.since,
            Some(5000),
            "since stays pinned to the episode start"
        );

        // 60 s after the last notification: notify again.
        let c = alarm.evaluate(&lost_not_locked(), t0 + Duration::from_secs(60), 5060);
        assert!(c.emit_notification, "cadence elapsed → notify again");
        assert!(!c.log_lost);

        // Just after: silent again until the next cadence boundary.
        let d = alarm.evaluate(&lost_not_locked(), t0 + Duration::from_secs(75), 5075);
        assert!(!d.emit_notification);
        let e = alarm.evaluate(&lost_not_locked(), t0 + Duration::from_secs(120), 5120);
        assert!(e.emit_notification);
    }

    #[test]
    fn regained_logs_once_and_resets_episode() {
        let mut alarm = ClockAlarm::from_interval_secs(60);
        let t0 = Instant::now();
        let _ = alarm.evaluate(&lost_not_locked(), t0, 7000);
        assert!(alarm.is_active());

        let regain = alarm.evaluate(&healthy(), t0 + Duration::from_secs(10), 7010);
        assert!(regain.log_regained, "clearing the alarm logs REGAINED once");
        assert!(!regain.emit_notification);
        assert!(!regain.snapshot.active);
        assert_eq!(regain.snapshot.since, None);
        assert!(!alarm.is_active());

        // A second healthy tick does NOT re-log REGAINED.
        let still = alarm.evaluate(&healthy(), t0 + Duration::from_secs(20), 7020);
        assert!(!still.log_regained);

        // A NEW lost episode gets a fresh `since` and a fresh immediate notify.
        let relost = alarm.evaluate(&lost_not_locked(), t0 + Duration::from_secs(30), 7030);
        assert!(relost.emit_notification);
        assert!(relost.log_lost);
        assert_eq!(relost.snapshot.since, Some(7030));
    }

    #[test]
    fn lost_reason_precedence_and_healthy_none() {
        // Healthy → None.
        assert_eq!(healthy().lost_reason(), None);

        // Unresolvable hostname wins over everything else, even if stale/unlocked.
        let h = ClockHealth {
            is_locked: false,
            mode_locked: false,
            gm_allowed: false,
            ptp_stale: true,
            allowlist_unresolvable: Some("video-clock.lan".to_string()),
            in_acquisition: false,
        };
        assert_eq!(
            h.lost_reason().as_deref(),
            Some("grandmaster hostname video-clock.lan unresolvable")
        );

        // Staleness beats not-allowed and not-locked.
        let stale = ClockHealth {
            is_locked: false,
            mode_locked: false,
            gm_allowed: false,
            ptp_stale: true,
            allowlist_unresolvable: None,
            in_acquisition: false,
        };
        assert_eq!(
            stale.lost_reason().as_deref(),
            Some("no PTP announce from an allowed grandmaster")
        );

        // Present but disallowed source (not stale).
        let disallowed = ClockHealth {
            is_locked: true,
            mode_locked: true,
            gm_allowed: false,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: false,
        };
        assert_eq!(
            disallowed.lost_reason().as_deref(),
            Some("PTP source not in the grandmaster allowlist")
        );

        // Plain not-locked (allowed source, not stale, but servo not locked).
        assert_eq!(
            lost_not_locked().lost_reason().as_deref(),
            Some("not PTP-locked to the grandmaster")
        );

        // A locked-mode-but-not-is_locked mix is still lost.
        let half = ClockHealth {
            is_locked: true,
            mode_locked: false,
            gm_allowed: true,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: false,
        };
        assert!(half.lost_reason().is_some());
    }

    #[test]
    fn in_acquisition_suppresses_not_locked_but_never_hard_failures() {
        // Boot acquisition, packets flowing, not yet locked → suppressed (no
        // spurious reboot alarm).
        let acquiring = ClockHealth {
            is_locked: false,
            mode_locked: false,
            gm_allowed: false,
            ptp_stale: false,
            allowlist_unresolvable: None,
            in_acquisition: true,
        };
        assert_eq!(
            acquiring.lost_reason(),
            None,
            "a not-yet-locked clock during boot acquisition must not alarm"
        );

        // But a HARD failure during acquisition still fires: no packets at all…
        let stale_at_boot = ClockHealth {
            ptp_stale: true,
            in_acquisition: true,
            ..acquiring.clone()
        };
        assert_eq!(
            stale_at_boot.lost_reason().as_deref(),
            Some("no PTP announce from an allowed grandmaster"),
            "no PTP packets at all is a real loss even during acquisition"
        );

        // …and an unresolvable hostname at boot.
        let unresolvable_at_boot = ClockHealth {
            allowlist_unresolvable: Some("video-clock.lan".to_string()),
            in_acquisition: true,
            ..acquiring.clone()
        };
        assert_eq!(
            unresolvable_at_boot.lost_reason().as_deref(),
            Some("grandmaster hostname video-clock.lan unresolvable"),
            "an unresolvable hostname is a real failure even during acquisition"
        );

        // Once the grace is over (in_acquisition=false), a still-not-locked clock
        // DOES alarm — the real "never locked" problem is not hidden forever.
        let past_grace = ClockHealth {
            in_acquisition: false,
            ..acquiring
        };
        assert_eq!(
            past_grace.lost_reason().as_deref(),
            Some("not PTP-locked to the grandmaster")
        );
    }

    #[test]
    fn from_interval_secs_floors_nonsense() {
        // 0 would make the alarm notify every single tick — must floor.
        let mut alarm = ClockAlarm::from_interval_secs(0);
        let t0 = Instant::now();
        assert!(alarm.evaluate(&lost_not_locked(), t0, 1).emit_notification);
        // 5 s later: still inside the 10 s floor → silent (proves it did not become 0).
        assert!(
            !alarm
                .evaluate(&lost_not_locked(), t0 + Duration::from_secs(5), 6)
                .emit_notification
        );
        // 10 s: floor elapsed → notify.
        assert!(
            alarm
                .evaluate(&lost_not_locked(), t0 + Duration::from_secs(10), 11)
                .emit_notification
        );
    }

    #[test]
    fn compose_message_formats_or_degrades() {
        let with_epoch = compose_message("not PTP-locked to the grandmaster", Some(1_700_000_000));
        assert!(with_epoch.starts_with(
            "NO DANTE CLOCK — not PTP-locked to the grandmaster, running on NTP fallback since "
        ));
        // HH:MM shape at the end.
        let tail = with_epoch.rsplit(" since ").next().unwrap();
        assert_eq!(tail.len(), 5, "expected HH:MM, got {tail:?}");
        assert_eq!(tail.as_bytes()[2], b':');

        let none = compose_message("reason", None);
        assert!(none.ends_with("since just now"));
    }

    #[test]
    fn clock_alarm_status_serde_roundtrip() {
        let s = ClockAlarmStatus {
            active: true,
            since: Some(123),
            reason: "not PTP-locked to the grandmaster".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ClockAlarmStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);

        // Inactive serializes `since` as an explicit null.
        let inactive = ClockAlarmStatus::default();
        let json = serde_json::to_string(&inactive).unwrap();
        assert!(json.contains("\"since\":null"), "got {json}");
        assert!(json.contains("\"active\":false"));
    }

    struct CountingNotifier {
        calls: RefCell<Vec<String>>,
    }
    impl ClockAlarmNotifier for CountingNotifier {
        fn notify(&self, _title: &str, message: &str) {
            self.calls.borrow_mut().push(message.to_string());
        }
    }

    #[test]
    fn drive_calls_notifier_once_per_cadence_and_never_while_healthy() {
        let notifier = CountingNotifier {
            calls: RefCell::new(Vec::new()),
        };
        let mut alarm = ClockAlarm::from_interval_secs(60);
        let t0 = Instant::now();

        // Healthy → no notify.
        drive(&mut alarm, &healthy(), t0, 0, &notifier);
        assert_eq!(notifier.calls.borrow().len(), 0);

        // Lost at t0 → 1 notify (message carries the reason).
        drive(
            &mut alarm,
            &lost_not_locked(),
            t0 + Duration::from_secs(1),
            1,
            &notifier,
        );
        assert_eq!(notifier.calls.borrow().len(), 1);
        assert!(notifier.calls.borrow()[0].contains("not PTP-locked"));

        // +30 s → still 1 (inside cadence).
        drive(
            &mut alarm,
            &lost_not_locked(),
            t0 + Duration::from_secs(31),
            31,
            &notifier,
        );
        assert_eq!(notifier.calls.borrow().len(), 1);

        // +61 s → 2.
        drive(
            &mut alarm,
            &lost_not_locked(),
            t0 + Duration::from_secs(61),
            61,
            &notifier,
        );
        assert_eq!(notifier.calls.borrow().len(), 2);

        // Regained → still 2 (no notify on regain).
        drive(
            &mut alarm,
            &healthy(),
            t0 + Duration::from_secs(70),
            70,
            &notifier,
        );
        assert_eq!(notifier.calls.borrow().len(), 2);
    }
}
