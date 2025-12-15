use anyhow::Result;
use rsntp::SntpClient;
use std::time::Duration;

pub struct NtpClient {
    server: String,
}

impl NtpClient {
    pub fn new(server: &str) -> Self {
        NtpClient {
            server: server.to_string(),
        }
    }

    /// Fetches the current time from the NTP server.
    /// Returns the offset required to apply to the local system time (Local + Offset = True Time).
    /// Positive offset means local clock is behind (needs to step forward).
    pub fn get_offset(&self) -> Result<(Duration, i8)> {
        let client = SntpClient::new();
        let result = client.synchronize(&self.server)?;
        
        let offset = result.clock_offset(); 
        let offset_secs = offset.as_secs_f64();
        
        let sign = if offset_secs < 0.0 { -1 } else { 1 };
        let abs_secs = offset_secs.abs();
        
        // Convert abs_secs to Duration
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        
        Ok((Duration::new(secs, nanos), sign))
    }
}