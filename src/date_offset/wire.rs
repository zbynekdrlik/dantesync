//! The wire codec of the 31900 time-query reply's date-offset extension (dantesync#88, v2 #119,
//! v3 the #119 micro-corrections). Pure, like the rest of `crate::date_offset`.

use super::{clamp_slew_ppm, DateAnnounce, SlewSpec};

/// Version of the 31900 reply extension carried by this build (2 = v1 + the #119 slew fields,
/// 3 = v2 + the MICRO flag).
pub const EXT_VERSION: u8 = 3;

/// Size of the v1 extension appended after the 64-byte base reply.
///
/// Layout (big-endian, offsets relative to the start of the extension = byte 64 of the reply):
///
/// ```text
/// [0]      version (1)
/// [1]      flags: bit 0 = this node is the date-offset AUTHORITY (the NTP master)
/// [2-3]    reserved (zero)
/// [4-11]   date_offset_ns   (i64) — D, where wall = PTP time + D
/// [12-19]  effective_ptp_ns (i64) — the PTP instant D takes effect (future = a pending step)
/// [20-23]  seq              (u32) — bumped on every change of D
/// [24-29]  gm_uuid          (6 bytes) — the grandmaster whose PTP time base D belongs to
/// [30-31]  reserved (zero)
/// [32-39]  now_ptp_ns       (i64) — the replying node's PTP time when it built the reply
///                            (its wall − its D IN EFFECT), for the follower's time-base check
/// ```
///
/// `gm_uuid` is the grandmaster of the authority's ANCHOR, not "the grandmaster I hear right now":
/// during a grandmaster change those differ for a window, and publishing the new UUID beside an
/// old-base `D` would let a follower that already re-anchored adopt a days-wrong offset.
///
/// A future version APPENDS fields; a v1 reader decodes the first 40 bytes of any version ≥ 1.
///
/// Version 2 (dantesync#119) keeps those 40 bytes and uses two of them that v1 writes as zero, so a
/// v1 reader simply sees a step (the pre-#119 behaviour):
///
/// ```text
/// [1]      flags: bit 1 = the announce is a SLEW (`date_offset_ns` is where it ends,
///                          `effective_ptp_ns` where it starts)
/// [2-3]    slew_ppm         (u16) — the slew rate, 0 unless bit 1 is set
/// [40-47]  slew_from_ns     (i64) — D where the slew starts
/// ```
///
/// Version 3 (dantesync#119 follow-up) keeps the v2 layout and sets one more flag, which a v2 reader
/// ignores (it simply takes the increment as a plain step or slew):
///
/// ```text
/// [1]      flags: bit 2 = the announce is a MICRO-correction (a sub-threshold increment)
/// ```
pub const EXT_SIZE: usize = 40;

/// Size of the v2 extension (dantesync#119): the v1 fields plus `slew_from_ns`. This build writes
/// it; a reply is only read as a slew when the version is ≥ 2 AND all of it is present.
pub const EXT_SIZE_V2: usize = 48;

/// Extension flag: the replying node is the fleet's date-offset authority.
pub const EXT_FLAG_AUTHORITY: u8 = 0x01;

/// Extension flag (v2, dantesync#119): the announce is a coordinated SLEW, not a step.
pub const EXT_FLAG_SLEW: u8 = 0x02;

/// Extension flag (v3, dantesync#119 follow-up): the announce is a MICRO-correction.
pub const EXT_FLAG_MICRO: u8 = 0x04;

/// The decoded 31900 reply extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateExtension {
    pub version: u8,
    /// True only on the fleet's date-offset authority. A client that is not the authority still
    /// mirrors its own state here (observability), with this flag clear — a follower must never
    /// adopt it.
    pub authority: bool,
    pub announce: DateAnnounce,
    /// The grandmaster whose PTP time base `announce.date_offset_ns` belongs to.
    pub gm_uuid: [u8; 6],
    /// The replying node's PTP time when it built the reply (its wall − its `D` in effect).
    pub now_ptp_ns: i64,
}

/// Encode the v3 extension (see [`EXT_SIZE`] for the layout).
pub fn encode_extension(ext: &DateExtension) -> [u8; EXT_SIZE_V2] {
    let mut out = [0u8; EXT_SIZE_V2];
    out[0] = EXT_VERSION;
    out[1] = if ext.authority { EXT_FLAG_AUTHORITY } else { 0 };
    out[4..12].copy_from_slice(&ext.announce.date_offset_ns.to_be_bytes());
    out[12..20].copy_from_slice(&ext.announce.effective_ptp_ns.to_be_bytes());
    out[20..24].copy_from_slice(&ext.announce.seq.to_be_bytes());
    out[24..30].copy_from_slice(&ext.gm_uuid);
    out[32..40].copy_from_slice(&ext.now_ptp_ns.to_be_bytes());
    if let Some(slew) = ext.announce.slew {
        out[1] |= EXT_FLAG_SLEW;
        let ppm = clamp_slew_ppm(slew.ppm).min(u16::MAX as u32) as u16;
        out[2..4].copy_from_slice(&ppm.to_be_bytes());
        out[40..48].copy_from_slice(&slew.from_ns.to_be_bytes());
    }
    if ext.announce.micro {
        out[1] |= EXT_FLAG_MICRO;
    }
    out
}

/// Decode an extension. `None` when it is absent (fewer than [`EXT_SIZE`] bytes — an older
/// server's plain 64-byte reply) or when the version byte is 0 (never a valid extension).
/// Any version ≥ 1 is accepted and read as v1: later versions only append fields. The #119 slew
/// is read only from a version ≥ 2 extension that carries all [`EXT_SIZE_V2`] bytes; its rate is
/// clamped like a configured one. The MICRO kind is read only from a version ≥ 3 extension.
pub fn decode_extension(bytes: &[u8]) -> Option<DateExtension> {
    if bytes.len() < EXT_SIZE || bytes[0] == 0 {
        return None;
    }
    let i64_at = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[at..at + 8]);
        i64::from_be_bytes(b)
    };
    let slew =
        (bytes[0] >= 2 && bytes.len() >= EXT_SIZE_V2 && bytes[1] & EXT_FLAG_SLEW != 0).then(|| {
            SlewSpec {
                from_ns: i64_at(40),
                ppm: clamp_slew_ppm(u16::from_be_bytes([bytes[2], bytes[3]]) as u32),
            }
        });
    Some(DateExtension {
        version: bytes[0],
        authority: bytes[1] & EXT_FLAG_AUTHORITY != 0,
        announce: DateAnnounce {
            date_offset_ns: i64_at(4),
            effective_ptp_ns: i64_at(12),
            seq: u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]),
            slew,
            micro: bytes[0] >= 3 && bytes[1] & EXT_FLAG_MICRO != 0,
        },
        gm_uuid: [
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29],
        ],
        now_ptp_ns: i64_at(32),
    })
}
