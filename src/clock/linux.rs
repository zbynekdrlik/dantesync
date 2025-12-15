use super::SystemClock;
use anyhow::{Result, anyhow};
use libc::{self, timex, adjtimex, ADJ_FREQUENCY};
use std::mem;

pub struct LinuxClock {
    original_freq: i64,
}

impl LinuxClock {
    pub fn new() -> Result<Self> {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = 0; // Query mode
        
        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
            return Err(anyhow!("adjtimex failed (are you root?)"));
        }

        Ok(LinuxClock {
            original_freq: tx.freq,
        })
    }
}

impl SystemClock for LinuxClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let ppm = (factor - 1.0) * 1_000_000.0;
        // freq is in ppm with 16-bit fractional part.
        // So 1 ppm = 65536.
        // Range is usually +/- 500 ppm.
        
        let freq_val = (ppm * 65536.0) as i64;
        
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = freq_val;

        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
             return Err(anyhow!("adjtimex failed to set frequency"));
        }
        
        Ok(())
    }
}

impl Drop for LinuxClock {
    fn drop(&mut self) {
        // Restore original frequency
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = self.original_freq;
        unsafe { adjtimex(&mut tx) };
    }
}
