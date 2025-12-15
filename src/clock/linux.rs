use super::SystemClock;
use anyhow::{Result, anyhow};
use libc::{self, timex, adjtimex, ADJ_FREQUENCY, timeval, settimeofday};
use std::mem;
use std::time::Duration;

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

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let mut tv: timeval = unsafe { mem::zeroed() };
        unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };

        let offset_sec = offset.as_secs() as i64;
        let offset_usec = offset.subsec_micros() as i64;

        if sign > 0 {
            tv.tv_sec += offset_sec;
            tv.tv_usec += offset_usec;
        } else {
            tv.tv_sec -= offset_sec;
            tv.tv_usec -= offset_usec;
        }

        // Normalize
        while tv.tv_usec >= 1_000_000 {
            tv.tv_sec += 1;
            tv.tv_usec -= 1_000_000;
        }
        while tv.tv_usec < 0 {
            tv.tv_sec -= 1;
            tv.tv_usec += 1_000_000;
        }

        let ret = unsafe { settimeofday(&tv, std::ptr::null()) };
        if ret < 0 {
            return Err(anyhow!("settimeofday failed"));
        }
        Ok(())
    }
}

impl Drop for LinuxClock {
    fn drop(&mut self) {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = self.original_freq;
        unsafe { adjtimex(&mut tx) };
    }
}