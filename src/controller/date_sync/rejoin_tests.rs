//! dantesync#112 — a PTP loss, the multicast re-join and the re-acquisition never re-derive the
//! fleet date offset on the NTP master.
//!
//! The 29.9.2026 strih-lx case: the fleet's date master lost PTP at a USB NIC re-plug, and only a
//! service RESTART recovered it. A fresh process anchors `D` on its own NTP-stepped wall and makes
//! that the new authority `D`: it moved by −19.8 ms and the whole fleet stepped. In-process `D` is
//! anchored once (`AnchorEvent::Anchored`, the first lock). A re-engagement after an outage is
//! `Realigned`: only the master's own anchor moves, and it steps its OWN wall back onto the fleet
//! line (one Join step), so the fleet `D` and `date_offset_seq` stay as they were.

use super::tests::{anchored_controller_with, phase_lock_config, PL_PTP_NOW_NS};
use super::*;
use crate::clock::MockSystemClock;
use crate::traits::{MockNtpSource, RejoinOutcome};
use std::sync::atomic::{AtomicU32, Ordering};

const S: i64 = 1_000_000_000;

/// The phase-lock config in daily mode (the production default, with its nightly window 6 h away
/// so it cannot open during the test) or in micro mode.
fn config(daily: bool) -> SystemConfig {
    let mut config = phase_lock_config();
    if daily {
        let tod = (wall_now_ns() / S + 6 * 3_600).rem_euclid(86_400);
        config.date_offset.correction = "daily".to_string();
        config.date_offset.daily_step_utc =
            format!("{:02}:{:02}:{:02}", tod / 3_600, tod % 3_600 / 60, tod % 60);
    }
    config
}

#[test]
fn a_ptp_loss_rejoin_and_re_acquire_keep_the_fleet_d_and_its_seq_on_the_master_112() {
    for daily in [true, false] {
        // The one clock step allowed: the master's OWN wall back onto the fleet line after the
        // free-run (a Join of −3 ms).
        let mut clock = MockSystemClock::new();
        clock
            .expect_step_clock()
            .times(1)
            .withf(|dur, sign| *dur == Duration::from_millis(3) && *sign == -1)
            .returning(|_, _| Ok(()));
        let (mut c, d) = anchored_controller_with(clock, MockNtpSource::new(), true, config(daily));
        let rejoins = Arc::new(AtomicU32::new(0));
        let seen = rejoins.clone();
        c.network.expect_rejoin().returning(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(RejoinOutcome {
                iface: "enx002427159965".to_string(),
                ip: std::net::Ipv4Addr::new(10, 77, 9, 202),
                changed: false,
            })
        });
        c.note_allowed_ptp_packet(Instant::now());
        c.update_shared_status();
        let seq0 = {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert!(st.is_locked);
            assert_eq!(st.date_offset_ns, Some(d));
            st.date_offset_seq
        };
        assert!(seq0.is_some(), "the master publishes its authority's seq");

        // The NIC is re-plugged: no PTP packet for 11 s. The loop re-joins; no restart. (An
        // `Instant` cannot move forward, so the receive history is re-written in time order:
        // the last packet came 11 s ago.)
        c.ptp_liveness.rx = crate::ptp_rejoin::RxWindow::new();
        c.note_allowed_ptp_packet(Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 1));
        c.check_ptp_status();
        c.maybe_rejoin_ptp(Instant::now());
        assert_eq!(
            rejoins.load(Ordering::SeqCst),
            1,
            "daily={daily}: re-joined"
        );
        c.service_date_offset();
        c.update_shared_status();
        {
            let st = c.get_status_shared();
            let st = st.read().expect("status");
            assert!(!st.is_locked, "daily={daily}: stale is not locked");
            assert_eq!(st.mode, "NTP-only");
            assert_eq!(
                st.date_offset_ns,
                Some(d),
                "daily={daily}: the fleet D holds through the outage"
            );
            assert_eq!(st.date_offset_seq, seq0);
        }

        // The re-join worked: PTP is back, and the wall free-ran 3 ms off the fleet line.
        c.note_allowed_ptp_packet(Instant::now());
        c.check_ptp_status();
        c.date_sync.pending_median_ns = Some(d + 3_000_000);
        c.date_sync.pending_t1_ns = PL_PTP_NOW_NS + 30 * S;
        c.apply_self_tuning_servo(0.0);
        assert!(
            c.date_sync.core.engaged(),
            "daily={daily}: the phase lock re-engaged in the same process"
        );
        c.service_date_offset();
        c.update_shared_status();
        let st = c.get_status_shared();
        let st = st.read().expect("status");
        assert!(st.is_locked, "daily={daily}: LOCK again");
        assert_eq!(st.mode, "LOCK");
        assert_eq!(
            st.date_offset_ns,
            Some(d),
            "daily={daily}: D is not re-derived at re-lock"
        );
        assert_eq!(
            st.date_offset_seq, seq0,
            "daily={daily}: no new fleet announce"
        );
        assert_eq!(
            st.date_step_pending_ns, None,
            "daily={daily}: nothing for the fleet to step"
        );
        assert_eq!(
            st.last_date_step_kind, "join",
            "daily={daily}: the only step was the master's own re-join"
        );
        assert_eq!(
            c.date_sync
                .authority
                .as_ref()
                .expect("authority")
                .announce()
                .date_offset_ns,
            d
        );
    }
}
