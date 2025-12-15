use std::io::Cursor;
use byteorder::{BigEndian, ReadBytesExt};
use anyhow::{Result, anyhow};

pub const PTP_MULTICAST_ADDR: &str = "224.0.1.129";
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

#[derive(Debug)]
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
        
        let msg_type_val = rdr.read_u8()?;
        let _src_comm_tech = rdr.read_u8()?;
        
        let mut source_uuid = [0u8; 6];
        for i in 0..6 {
            source_uuid[i] = rdr.read_u8()?;
        }
        
        let _source_port_id = rdr.read_u16::<BigEndian>()?;
        let sequence_id = rdr.read_u16::<BigEndian>()?;
        let control = rdr.read_u8()?;
        
        // Remaining: reservedByte33, flags...

        // Map control field to enum if it matches message type, 
        // but the C++ code checks 'control' byte specifically for Sync/FollowUp.
        // Actually, in PTPv1, the 'messageTypeValue' often aligns with 'control' for the main types.
        // We'll use the 'control' field as the primary type indicator as per C++ reference:
        // enum class PtpV1Control : uint8_t { ... }
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

#[derive(Debug)]
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
pub struct PtpV1FollowUpBody {
    pub associated_sequence_id: u16,
    pub precise_origin_timestamp: PtpTimestamp,
}

impl PtpV1FollowUpBody {
    // 2 bytes padding + 2 bytes seq + 8 bytes timestamp = 12 bytes minimum
    // But struct in C++ has 'uint8_t reserved_padding1[6]' (6 bytes)
    // Then 'uint16_t associatedSequenceId'
    // Then 'PtpTimestamp preciseOriginTimestamp' (8 bytes)
    // Total = 6 + 2 + 8 = 16 bytes.
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
