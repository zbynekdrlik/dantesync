use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use std::io::Cursor;

pub const PTP_EVENT_PORT: u16 = 319;
pub const PTP_GENERAL_PORT: u16 = 320;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PtpV1Control {
    Sync = 0,
    DelayReq = 1,
    FollowUp = 2,
    DelayResp = 3,
    Management = 4,
    Other = 5,
}

impl From<u8> for PtpV1Control {
    fn from(v: u8) -> Self {
        match v {
            0 => PtpV1Control::Sync,
            1 => PtpV1Control::DelayReq,
            2 => PtpV1Control::FollowUp,
            3 => PtpV1Control::DelayResp,
            4 => PtpV1Control::Management,
            _ => PtpV1Control::Other,
        }
    }
}

/// dantesync#129 — what every node before 1.17 read as the grandmaster UUID off a real Dante Sync:
/// a 36-byte header plus a 13-byte skip landed on absolute bytes 49..55, the reserved byte and
/// `grandmasterCommunicationTechnology` (1) inside zeros. It was reported on `/status.gm_uuid`,
/// saved by the NTP master with its date offset (`date-offset.json`) and announced on 31900, so it
/// is a value a newer node still meets: it names no grandmaster.
pub const LEGACY_MISREAD_GM_UUID: [u8; 6] = [0, 0, 0, 0, 1, 0];

/// The PTPv1 message header (IEEE 1588-2002), 40 bytes:
///
/// | Offset | Field |
/// |---|---|
/// | 0 | versionPTP (u16) |
/// | 2 | versionNetwork (u16) |
/// | 4..20 | subdomain |
/// | 20 | messageType |
/// | 21 | sourceCommunicationTechnology |
/// | 22..28 | sourceUuid |
/// | 28 | sourcePortId (u16) |
/// | 30 | sequenceId (u16) |
/// | 32 | control |
/// | 33 | reserved |
/// | 34 | flags (u16) |
/// | 36..40 | reserved |
///
/// dantesync#129: the size was 36 before 1.17, so the Sync body was read 4 bytes early.
#[derive(Debug, PartialEq, Eq)]
pub struct PtpV1Header {
    pub version_ptp: u16,
    pub version_network: u16,
    /// The subdomain name, NUL-padded (Dante: `_DFLT`).
    pub subdomain: [u8; 16],
    /// Taken from `control` (the message's kind), not from the header's `messageType` byte.
    pub message_type: PtpV1Control,
    pub source_uuid: [u8; 6],
    pub source_port_id: u16,
    pub sequence_id: u16,
    pub control: u8,
}

impl PtpV1Header {
    pub const SIZE: usize = 40;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(anyhow!("Packet too short for PTP header"));
        }
        let mut rdr = Cursor::new(data);

        let version_ptp = rdr.read_u16::<BigEndian>()?;
        let version_network = rdr.read_u16::<BigEndian>()?;

        let mut subdomain = [0u8; 16];
        for byte in &mut subdomain {
            *byte = rdr.read_u8()?;
        }

        let _msg_type_val = rdr.read_u8()?;
        let _src_comm_tech = rdr.read_u8()?;

        let mut source_uuid = [0u8; 6];
        for byte in &mut source_uuid {
            *byte = rdr.read_u8()?;
        }

        let source_port_id = rdr.read_u16::<BigEndian>()?;
        let sequence_id = rdr.read_u16::<BigEndian>()?;
        let control = rdr.read_u8()?;
        // reserved (1), flags (2), reserved (4): nothing read.

        let message_type = PtpV1Control::from(control);

        Ok(PtpV1Header {
            version_ptp,
            version_network,
            subdomain,
            message_type,
            source_uuid,
            source_port_id,
            sequence_id,
            control,
        })
    }

    /// The subdomain as text: up to the first NUL, lossy UTF-8 (a log / `/status` value).
    pub fn subdomain_name(&self) -> String {
        nul_terminated(&self.subdomain)
    }
}

fn nul_terminated(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// dantesync#112 — a PTP TIME message: a datagram with a whole PTPv1 header whose control is Sync
/// or Follow_Up, i.e. the grandmaster's time. The ONE definition of "PTP was received": the
/// controller's liveness (and the grandmaster it names) and the backends' home address both use
/// it, so a runt, another follower's Delay_Req or any stray datagram on 319/320 counts for neither.
pub fn is_time_message(buf: &[u8]) -> bool {
    buf.len() >= PtpV1Header::SIZE
        && matches!(
            PtpV1Control::from(buf[32]),
            PtpV1Control::Sync | PtpV1Control::FollowUp
        )
}

#[derive(Debug, PartialEq, Eq)]
pub struct PtpTimestamp {
    pub seconds: u32,
    pub nanoseconds: u32,
}

impl PtpTimestamp {
    /// Convert timestamp to nanoseconds.
    /// Uses saturating arithmetic to prevent overflow from malformed packets.
    /// Note: Dante PTP uses device uptime (not Unix epoch), so seconds values
    /// are typically small, but we handle edge cases defensively.
    pub fn to_nanos(&self) -> i64 {
        (self.seconds as i64)
            .saturating_mul(1_000_000_000)
            .saturating_add(self.nanoseconds as i64)
    }
}

/// The grandmaster block of a PTPv1 Sync body (IEEE 1588-2002), offsets from the header's end:
///
/// | Offset | Field |
/// |---|---|
/// | 0..8 | originTimestamp |
/// | 8 | epochNumber (u16) |
/// | 10 | currentUTCOffset (i16) |
/// | 12 | reserved |
/// | 13 | grandmasterCommunicationTechnology |
/// | 14..20 | grandmasterClockUuid |
/// | 20 | grandmasterPortId (u16) |
/// | 22 | grandmasterSequenceId (u16) |
/// | 24..27 | reserved |
/// | 27 | grandmasterClockStratum |
/// | 28..32 | grandmasterClockIdentifier |
/// | 32..34 | reserved |
/// | 34 | grandmasterClockVariance (i16) |
/// | 36 | reserved |
/// | 37 | grandmasterPreferred |
///
/// The rest of the 84-byte body (the local and parent clock) is not read. dantesync#129: the
/// best-master fields are parsed for the grandmaster failover; nothing selects on them yet.
#[derive(Debug, PartialEq, Eq)]
pub struct PtpV1SyncMessageBody {
    pub grandmaster_clock_uuid: [u8; 6],
    pub grandmaster_port_id: u16,
    pub grandmaster_sequence_id: u16,
    pub grandmaster_clock_stratum: u8,
    pub grandmaster_clock_identifier: [u8; 4],
    pub grandmaster_clock_variance: i16,
    pub grandmaster_preferred: bool,
}

impl PtpV1SyncMessageBody {
    /// Up to and including grandmasterPreferred.
    pub const MIN_SIZE: usize = 38;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::MIN_SIZE {
            return Err(anyhow!("Packet too short for Sync body"));
        }
        let mut rdr = Cursor::new(data);

        // originTimestamp (8), epochNumber (2), currentUTCOffset (2), reserved (1),
        // grandmasterCommunicationTechnology (1).
        rdr.set_position(14);

        let mut grandmaster_clock_uuid = [0u8; 6];
        for byte in &mut grandmaster_clock_uuid {
            *byte = rdr.read_u8()?;
        }
        let grandmaster_port_id = rdr.read_u16::<BigEndian>()?;
        let grandmaster_sequence_id = rdr.read_u16::<BigEndian>()?;

        rdr.set_position(27);
        let grandmaster_clock_stratum = rdr.read_u8()?;
        let mut grandmaster_clock_identifier = [0u8; 4];
        for byte in &mut grandmaster_clock_identifier {
            *byte = rdr.read_u8()?;
        }

        rdr.set_position(34);
        let grandmaster_clock_variance = rdr.read_i16::<BigEndian>()?;

        rdr.set_position(37);
        let grandmaster_preferred = rdr.read_u8()? != 0;

        Ok(PtpV1SyncMessageBody {
            grandmaster_clock_uuid,
            grandmaster_port_id,
            grandmaster_sequence_id,
            grandmaster_clock_stratum,
            grandmaster_clock_identifier,
            grandmaster_clock_variance,
            grandmaster_preferred,
        })
    }

    /// The clock identifier as text (Dante: `DFLT`), as [`PtpV1Header::subdomain_name`].
    pub fn grandmaster_identifier_name(&self) -> String {
        nul_terminated(&self.grandmaster_clock_identifier)
    }
}

#[derive(Debug)]
pub struct PtpV1FollowUpBody {
    pub associated_sequence_id: u16,
    pub precise_origin_timestamp: PtpTimestamp,
}

impl PtpV1FollowUpBody {
    /// reserved (2), associatedSequenceId (2), preciseOriginTimestamp (8).
    pub const SIZE: usize = 12;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(anyhow!("Packet too short for FollowUp body"));
        }
        let mut rdr = Cursor::new(data);

        // reserved (2). dantesync#129: before 1.17 the body started 4 bytes early (a 36-byte
        // header) and skipped 6, which reached the same absolute byte 42: the decode is unchanged.
        rdr.set_position(2);

        let associated_sequence_id = rdr.read_u16::<BigEndian>()?;
        let seconds = rdr.read_u32::<BigEndian>()?;
        let nanoseconds = rdr.read_u32::<BigEndian>()?;

        Ok(PtpV1FollowUpBody {
            associated_sequence_id,
            precise_origin_timestamp: PtpTimestamp {
                seconds,
                nanoseconds,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dantesync#129 — a real Dante PTPv1 Sync (124 bytes, the UDP payload) and its Follow_Up
    /// (52 bytes), captured read-only off the wire: see `tests/fixtures/ptpv1/README.md`.
    const DANTE_SYNC: &[u8] = include_bytes!("../tests/fixtures/ptpv1/dante-sync.bin");
    const DANTE_FOLLOW_UP: &[u8] = include_bytes!("../tests/fixtures/ptpv1/dante-follow-up.bin");
    /// The grandmaster of the captured packets (an Audinate port, OUI 00:1d:c1).
    const DANTE_GM: [u8; 6] = [0x00, 0x1d, 0xc1, 0x08, 0x02, 0x14];

    #[test]
    fn test_ptp_v1_control_from() {
        assert_eq!(PtpV1Control::from(0), PtpV1Control::Sync);
        assert_eq!(PtpV1Control::from(1), PtpV1Control::DelayReq);
        assert_eq!(PtpV1Control::from(2), PtpV1Control::FollowUp);
        assert_eq!(PtpV1Control::from(3), PtpV1Control::DelayResp);
        assert_eq!(PtpV1Control::from(4), PtpV1Control::Management);
        assert_eq!(PtpV1Control::from(5), PtpV1Control::Other);
        assert_eq!(PtpV1Control::from(99), PtpV1Control::Other);
    }

    #[test]
    fn the_fixture_is_a_whole_dante_sync_and_its_follow_up_129() {
        assert_eq!(DANTE_SYNC.len(), 124, "a PTPv1 Sync is 124 bytes");
        assert_eq!(DANTE_FOLLOW_UP.len(), 52, "a PTPv1 Follow_Up is 52 bytes");
        assert_eq!(DANTE_SYNC[32], 0, "control = Sync");
        assert_eq!(DANTE_FOLLOW_UP[32], 2, "control = Follow_Up");
    }

    #[test]
    fn the_ptpv1_header_is_40_bytes_129() {
        // IEEE 1588-2002: the header ends with flags (34..36) and 4 reserved bytes (36..40);
        // the Follow_Up body (2 reserved + associatedSequenceId + preciseOriginTimestamp = 12
        // bytes) ends exactly at the datagram's 52 bytes.
        assert_eq!(PtpV1Header::SIZE, 40);
        assert_eq!(
            PtpV1Header::SIZE + PtpV1FollowUpBody::SIZE,
            DANTE_FOLLOW_UP.len()
        );
    }

    #[test]
    fn the_real_dante_header_decodes_129() {
        let h = PtpV1Header::parse(DANTE_SYNC).expect("a whole header");
        assert_eq!(h.version_ptp, 1, "versionPTP is a u16 at 0..2");
        assert_eq!(h.version_network, 1, "versionNetwork is a u16 at 2..4");
        assert_eq!(h.subdomain_name(), "_DFLT", "the Dante default subdomain");
        assert_eq!(h.source_uuid, DANTE_GM);
        assert_eq!(h.source_port_id, 2);
        assert_eq!(h.sequence_id, 0xb27a);
        assert_eq!(h.control, 0);
        assert_eq!(h.message_type, PtpV1Control::Sync);

        let f = PtpV1Header::parse(DANTE_FOLLOW_UP).expect("a whole header");
        assert_eq!(f.message_type, PtpV1Control::FollowUp);
        assert_eq!(f.sequence_id, 0xb27a);
        assert_eq!(f.source_uuid, DANTE_GM);
    }

    #[test]
    fn the_real_dante_sync_names_its_grandmaster_129() {
        let body = PtpV1SyncMessageBody::parse(&DANTE_SYNC[PtpV1Header::SIZE..])
            .expect("a whole Sync body");
        assert_eq!(
            body.grandmaster_clock_uuid, DANTE_GM,
            "grandmasterClockUuid is at body 14..20 (absolute 54..60)"
        );
        assert_ne!(body.grandmaster_clock_uuid, LEGACY_MISREAD_GM_UUID);
    }

    #[test]
    fn the_real_dante_sync_carries_the_best_master_fields_129() {
        let b = PtpV1SyncMessageBody::parse(&DANTE_SYNC[PtpV1Header::SIZE..])
            .expect("a whole Sync body");
        assert_eq!(b.grandmaster_port_id, 0);
        assert_eq!(b.grandmaster_sequence_id, 0xb27a);
        assert_eq!(b.grandmaster_clock_stratum, 0x79);
        assert_eq!(&b.grandmaster_clock_identifier, b"DFLT");
        assert_eq!(b.grandmaster_identifier_name(), "DFLT");
        assert_eq!(b.grandmaster_clock_variance, -4000);
        assert!(b.grandmaster_preferred);
    }

    #[test]
    fn the_old_offsets_read_the_legacy_constant_off_every_real_sync_129() {
        // Header SIZE 36 + a 13-byte skip = absolute 49..55: the reserved byte 52 and
        // grandmasterCommunicationTechnology (53) = 1 inside zeros. Every pre-1.17 node reported
        // it as its grandmaster's UUID (and saved it, and announced it on 31900).
        assert_eq!(DANTE_SYNC[49..55], LEGACY_MISREAD_GM_UUID);
    }

    #[test]
    fn the_real_follow_up_decode_is_byte_identical_129() {
        // The absolute offsets 42..44 / 44..48 / 48..52 are what every version decoded (36 + 6
        // before 1.17, 40 + 2 since): the precise origin timestamp must not move by a byte.
        let body = PtpV1FollowUpBody::parse(&DANTE_FOLLOW_UP[PtpV1Header::SIZE..])
            .expect("a whole Follow_Up body");
        assert_eq!(body.associated_sequence_id, 0xb27a);
        assert_eq!(body.precise_origin_timestamp.seconds, 541_867);
        assert_eq!(body.precise_origin_timestamp.nanoseconds, 434_557_859);
        let be16 = |i: usize| u16::from_be_bytes([DANTE_FOLLOW_UP[i], DANTE_FOLLOW_UP[i + 1]]);
        let be32 = |i: usize| {
            u32::from_be_bytes([
                DANTE_FOLLOW_UP[i],
                DANTE_FOLLOW_UP[i + 1],
                DANTE_FOLLOW_UP[i + 2],
                DANTE_FOLLOW_UP[i + 3],
            ])
        };
        assert_eq!(body.associated_sequence_id, be16(42));
        assert_eq!(body.precise_origin_timestamp.seconds, be32(44));
        assert_eq!(body.precise_origin_timestamp.nanoseconds, be32(48));
        // It pairs with the Sync of the same sequence id.
        let sync = PtpV1Header::parse(DANTE_SYNC).expect("a whole header");
        assert_eq!(body.associated_sequence_id, sync.sequence_id);
    }

    #[test]
    fn a_time_message_is_a_whole_header_with_sync_or_follow_up_112() {
        let packet = |control: u8, len: usize| {
            let mut buf = vec![0u8; len];
            if len > 32 {
                buf[32] = control;
            }
            buf
        };
        assert!(is_time_message(&packet(0, 60)), "Sync");
        assert!(is_time_message(&packet(2, 44)), "Follow_Up");
        assert!(is_time_message(&packet(0, 40)), "a whole header is enough");
        assert!(!is_time_message(&packet(1, 60)), "Delay_Req");
        assert!(!is_time_message(&packet(3, 60)), "Delay_Resp");
        assert!(!is_time_message(&packet(4, 60)), "Management");
        assert!(!is_time_message(&packet(0, 39)), "a runt");
        assert!(!is_time_message(&[]), "nothing");
        assert!(is_time_message(DANTE_SYNC));
        assert!(is_time_message(DANTE_FOLLOW_UP));
        assert!(
            !is_time_message(&DANTE_SYNC[..39]),
            "a header cut short (#129)"
        );
    }

    #[test]
    fn test_parse_header_too_short() {
        assert!(PtpV1Header::parse(&[0u8; 39]).is_err());
        assert!(PtpV1Header::parse(&DANTE_SYNC[..39]).is_err());
    }

    #[test]
    fn test_parse_header_valid_sync() {
        // A synthetic PTPv1 Sync header in the IEEE 1588-2002 layout.
        let mut data = vec![0u8; 40];
        data[1] = 1; // versionPTP = 1 (u16 at 0..2)
        data[3] = 1; // versionNetwork = 1 (u16 at 2..4)
        data[4..9].copy_from_slice(b"_ALT1"); // subdomain (4..20)
        data[20] = 1; // messageType = event
        data[22..28].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]); // sourceUuid
        data[28] = 0x00;
        data[29] = 0x03; // sourcePortId = 3
        data[30] = 0x01;
        data[31] = 0x02; // sequenceId = 0x0102 = 258
        data[32] = 0; // control = Sync

        let header = PtpV1Header::parse(&data).unwrap();
        assert_eq!(header.version_ptp, 1);
        assert_eq!(header.version_network, 1);
        assert_eq!(header.subdomain_name(), "_ALT1");
        assert_eq!(header.message_type, PtpV1Control::Sync);
        assert_eq!(header.sequence_id, 258);
        assert_eq!(header.source_port_id, 3);
        assert_eq!(header.source_uuid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn a_subdomain_name_stops_at_the_first_nul_and_never_panics_129() {
        let mut data = vec![0u8; 40];
        assert_eq!(PtpV1Header::parse(&data).unwrap().subdomain_name(), "");
        data[4..20].copy_from_slice(&[0xFF; 16]); // not UTF-8, no NUL
        let name = PtpV1Header::parse(&data).unwrap().subdomain_name();
        assert_eq!(name.chars().count(), 16, "lossy, one replacement per byte");
    }

    #[test]
    fn test_ptp_timestamp_to_nanos() {
        let ts = PtpTimestamp {
            seconds: 1,
            nanoseconds: 500,
        };
        assert_eq!(ts.to_nanos(), 1_000_000_500);
    }

    #[test]
    fn test_parse_followup_body() {
        // The Follow_Up body from the header's end: reserved (0..2), associatedSequenceId (2..4),
        // preciseOriginTimestamp seconds (4..8) + nanoseconds (8..12).
        let mut data = vec![0u8; 12];
        data[3] = 0x05;
        data[7] = 0x0A; // 10 seconds
        data[10] = 0x01; // 256 nanos
        let body = PtpV1FollowUpBody::parse(&data).unwrap();
        assert_eq!(body.associated_sequence_id, 5);
        assert_eq!(body.precise_origin_timestamp.seconds, 10);
        assert_eq!(body.precise_origin_timestamp.nanoseconds, 256);
        assert!(PtpV1FollowUpBody::parse(&data[..11]).is_err());
    }

    #[test]
    fn test_parse_sync_body_gm_uuid() {
        // The Sync body from the header's end: originTimestamp (0..8), epochNumber (8..10),
        // currentUTCOffset (10..12), reserved (12), grandmasterCommunicationTechnology (13),
        // grandmasterClockUuid (14..20) … grandmasterPreferred (37).
        let mut data = vec![0u8; 38];
        data[13] = 1;
        data[14..20].copy_from_slice(&[0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
        data[37] = 0;
        let body = PtpV1SyncMessageBody::parse(&data).unwrap();
        assert_eq!(
            body.grandmaster_clock_uuid,
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
        );
        assert!(!body.grandmaster_preferred);
        assert!(
            PtpV1SyncMessageBody::parse(&data[..37]).is_err(),
            "the grandmaster block is read whole or not at all"
        );
    }
}
