use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SyncStatus {
    pub offset_ns: i64,
    pub drift_ppm: f64,
    pub gm_uuid: Option<[u8; 6]>,
    pub settled: bool,
    pub updated_ts: u64,
}

impl Default for SyncStatus {
    fn default() -> Self {
        SyncStatus {
            offset_ns: 0,
            drift_ppm: 0.0,
            gm_uuid: None,
            settled: false,
            updated_ts: 0,
        }
    }
}
