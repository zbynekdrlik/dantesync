//! dantesync#129 (slice 0) — the grandmaster IDENTITY consumers once `gm_uuid` is real. Before
//! 1.17 every node read the same constant (`ptp::LEGACY_MISREAD_GM_UUID`), so none of these checks
//! could tell grandmasters apart. Slice 0 is report-only for the fleet: the GM-change detector
//! starts working, and nothing that used to adopt the master's date offset stops adopting it.

use super::tests::{
    anchored_controller, authority_reply, authority_reply_in_effect, with_authority,
};
use super::*;
use crate::clock::MockSystemClock;
use crate::ptp::{PtpV1Header, LEGACY_MISREAD_GM_UUID};
use crate::traits::MockNtpSource;

/// A real Dante Sync, captured read-only (`tests/fixtures/ptpv1/README.md`).
const DANTE_SYNC: &[u8] = include_bytes!("../../../tests/fixtures/ptpv1/dante-sync.bin");
const DANTE_GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x08, 0x02, 0x14];
/// Another port of an Audinate clock: another MAC, so another PTPv1 UUID.
const OTHER_PORT: [u8; 6] = [0x00, 0x1d, 0xc1, 0x08, 0x02, 0x15];

fn feed_sync(
    c: &mut PtpController<MockSystemClock, crate::traits::MockPtpNetwork, MockNtpSource>,
    packet: &[u8],
) {
    let header = PtpV1Header::parse(packet).expect("a whole header");
    c.handle_sync_message(&header, packet, std::time::SystemTime::now());
}

#[test]
fn the_first_real_grandmaster_uuid_after_a_start_is_no_grandmaster_change_129() {
    // An upgraded node learns the identity from its first Sync: `None` -> the real UUID, never a
    // "GRANDMASTER UUID CHANGED" re-anchor (the same holds for the old constant it read before:
    // a restart starts from `None`).
    let (mut c, _) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
    c.current_gm_uuid = None;
    feed_sync(&mut c, DANTE_SYNC);
    assert_eq!(c.current_gm_uuid, Some(DANTE_GM));
    assert!(
        !c.date_sync.core.rebase_pending(),
        "learning the identity is no change"
    );
    feed_sync(&mut c, DANTE_SYNC);
    assert!(
        !c.date_sync.core.rebase_pending(),
        "the same grandmaster again"
    );
}

#[test]
fn a_real_grandmaster_uuid_change_re_anchors_129() {
    // The detector now tells grandmasters apart: the same sender relaying another grandmaster.
    let (mut c, _) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
    c.current_gm_uuid = None;
    feed_sync(&mut c, DANTE_SYNC);
    let mut other = DANTE_SYNC.to_vec();
    other[54..60].copy_from_slice(&OTHER_PORT);
    feed_sync(&mut c, &other);
    assert_eq!(c.current_gm_uuid, Some(OTHER_PORT));
    assert!(c.date_sync.core.rebase_pending(), "D is re-anchored");
}

#[test]
fn a_follower_adopts_its_own_time_base_under_another_grandmaster_uuid_129() {
    // The master (video VLAN) and an audio-VLAN follower may hear two ports of ONE clock: the same
    // PTP time base under two UUIDs. Slice 0 must not start refusing it: the time-base check
    // decides, the UUID is only reported.
    let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
    let slot = with_authority(&mut c);
    *slot.lock().unwrap() = Some(authority_reply(1, OTHER_PORT, d, 1_000_000_000, 3));
    c.service_date_offset();
    assert!(c.date_sync.follower.adopted());
    assert_eq!(c.date_sync.follower.adopted_seq(), Some(3));
}

#[test]
fn a_follower_adopts_the_misread_constant_a_1_16_master_announces_129() {
    // A mixed fleet during the rollout: a 1.16 master announces the constant it misread.
    let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
    let slot = with_authority(&mut c);
    *slot.lock().unwrap() = Some(authority_reply(
        1,
        LEGACY_MISREAD_GM_UUID,
        d,
        1_000_000_000,
        3,
    ));
    c.service_date_offset();
    assert!(c.date_sync.follower.adopted());
}

#[test]
fn another_time_base_is_never_adopted_whatever_the_grandmaster_uuid_129() {
    // No step_clock expectation: a step panics the mock.
    let three_days: i64 = 3 * 86_400 * 1_000_000_000;
    for gm in [OTHER_PORT, LEGACY_MISREAD_GM_UUID] {
        let (mut c, d) = anchored_controller(MockSystemClock::new(), MockNtpSource::new(), false);
        let slot = with_authority(&mut c);
        *slot.lock().unwrap() = Some(authority_reply_in_effect(
            1,
            gm,
            d - three_days,
            1,
            9,
            d - three_days,
        ));
        c.service_date_offset();
        assert!(!c.date_sync.follower.adopted(), "{gm:02x?}");
        assert_eq!(c.date_sync.core.anchor_ns(), Some(d));
    }
}
