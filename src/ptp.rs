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

#[derive(Debug, PartialEq, Eq)]
pub struct PtpV1Header {
    pub version_ptp: u8,
    pub message_length: u16,
    pub message_type: PtpV1Control,
    pub source_uuid: [u8; 6],
    pub sequence_id: u16,
    pub control: u8,
}

impl PtpV1Header {
    pub const SIZE: usize = 36;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(anyhow!("Packet too short for PTP header"));
        }
        let mut rdr = Cursor::new(data);

        let v_r1 = rdr.read_u8()?;
        let version_ptp = (v_r1 >> 4) & 0x0F;

        let _v_n_r2 = rdr.read_u8()?; // versionNetwork
        let message_length = rdr.read_u16::<BigEndian>()?;

        // Skip subdomain (16 bytes)
        rdr.set_position(rdr.position() + 16);

        let _msg_type_val = rdr.read_u8()?;
        let _src_comm_tech = rdr.read_u8()?;

        let mut source_uuid = [0u8; 6];
        for i in 0..6 {
            source_uuid[i] = rdr.read_u8()?;
        }

        let _source_port_id = rdr.read_u16::<BigEndian>()?;
        let sequence_id = rdr.read_u16::<BigEndian>()?;
        let control = rdr.read_u8()?;

        let message_type = PtpV1Control::from(control);

        Ok(PtpV1Header {
            version_ptp,
            message_length,
            message_type,
            source_uuid,
            sequence_id,
            control,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PtpTimestamp {
    pub seconds: u32,
    pub nanoseconds: u32,
}

impl PtpTimestamp {
    pub fn to_nanos(&self) -> i64 {
        self.seconds as i64 * 1_000_000_000 + self.nanoseconds as i64
    }
}

#[derive(Debug)]
pub struct PtpV1SyncMessageBody {
    // originTimestamp (8)
    // epochNumber (2)
    // currentUtcOffset (2)
    // grandmasterCommTech (1)
    pub grandmaster_clock_uuid: [u8; 6],
    // ... others ignored
}

impl PtpV1SyncMessageBody {
    // We only need up to GM UUID (offset 13 + 6 = 19 bytes)
    pub const MIN_SIZE: usize = 19;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::MIN_SIZE {
            return Err(anyhow!("Packet too short for Sync body"));
        }
        let mut rdr = Cursor::new(data);

        // Skip originTimestamp (8), epoch (2), utcOffset (2), commTech (1) = 13 bytes
        rdr.set_position(13);

        let mut gm_uuid = [0u8; 6];
        for i in 0..6 {
            gm_uuid[i] = rdr.read_u8()?;
        }

        Ok(PtpV1SyncMessageBody {
            grandmaster_clock_uuid: gm_uuid,
        })
    }
}

#[derive(Debug)]
pub struct PtpV1FollowUpBody {
    pub associated_sequence_id: u16,
    pub precise_origin_timestamp: PtpTimestamp,
}

impl PtpV1FollowUpBody {
    pub const SIZE: usize = 16;

    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < Self::SIZE {
            return Err(anyhow!("Packet too short for FollowUp body"));
        }
        let mut rdr = Cursor::new(data);

        // Skip padding (6 bytes)
        rdr.set_position(rdr.position() + 6);

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
    fn test_parse_header_too_short() {
        let data = [0u8; 35];
        assert!(PtpV1Header::parse(&data).is_err());
    }

    #[test]
    fn test_parse_header_valid_sync() {
        // Construct a mock PTPv1 Sync Header
        let mut data = vec![0u8; 36];
        data[0] = 0x10; // Version PTP = 1
        data[3] = 0; // msg length high
        data[32] = 0; // Control = Sync (Offset 32)

        // UUID
        data[22] = 0xAA;
        data[23] = 0xBB;
        data[24] = 0xCC;
        data[25] = 0xDD;
        data[26] = 0xEE;
        data[27] = 0xFF;

        // Sequence ID (bytes 30, 31)
        data[30] = 0x01;
        data[31] = 0x02; // 0x0102 = 258

        let header = PtpV1Header::parse(&data).unwrap();
        assert_eq!(header.version_ptp, 1);
        assert_eq!(header.message_type, PtpV1Control::Sync);
        assert_eq!(header.sequence_id, 258);
        assert_eq!(header.source_uuid, [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
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
        let mut data = vec![0u8; 16];
        // Padding 6 bytes (0..6)
        // Associated Seq ID (6,7)
        data[6] = 0x00;
        data[7] = 0x05;
        // Seconds (8..11)
        data[8] = 0x00;
        data[9] = 0x00;
        data[10] = 0x00;
        data[11] = 0x0A; // 10 seconds
                         // Nanos (12..15)
        data[12] = 0x00;
        data[13] = 0x00;
        data[14] = 0x01;
        data[15] = 0x00; // 256 nanos

        let body = PtpV1FollowUpBody::parse(&data).unwrap();
        assert_eq!(body.associated_sequence_id, 5);
        assert_eq!(body.precise_origin_timestamp.seconds, 10);
        assert_eq!(body.precise_origin_timestamp.nanoseconds, 256);
    }

    #[test]
    fn test_parse_sync_body_gm_uuid() {
        let mut data = vec![0u8; 20];
        // 13 bytes skip
        // 13: GM UUID start
        data[13] = 0x11;
        data[14] = 0x22;
        data[15] = 0x33;
        data[16] = 0x44;
        data[17] = 0x55;
        data[18] = 0x66;

        let body = PtpV1SyncMessageBody::parse(&data).unwrap();
        assert_eq!(
            body.grandmaster_clock_uuid,
            [0x11, 0x22, 0x33, 0x44, 0x55, 0x66]
        );
    }
}
