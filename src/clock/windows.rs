use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, SYSTEMTIME, FILETIME};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustment,
    GetSystemTimeAsFileTime, SetSystemTime
};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::Duration;
use log::info;

pub struct WindowsClock {
    original_adjustment: u64,
    original_increment: u64,
    original_disabled: BOOL,
    nominal_frequency: u64,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        // Reset any existing adjustments
        unsafe {
            let _ = SetSystemTimeAdjustmentPrecise(0, true);
            let _ = SetSystemTimeAdjustment(0, true);
        }

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            if let Err(e) = GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled) {
                log::warn!("GetSystemTimeAdjustmentPrecise failed, trying legacy. Error: {}", e);
                return Err(anyhow!("GetSystemTimeAdjustmentPrecise failed: {}", e));
            }
        }
        
        info!("Windows Clock Initial State: Adj={}, Inc={}, Disabled={}", adj, inc, disabled.as_bool());

        // Sanity check for VM/Hyper-V reporting huge increments (1s)
        if inc > 200_000 {
            log::warn!("Reported Time Increment {} is too large (>20ms). Suspect timer mismatch. Forcing standard 156,250 (15.625ms).", inc);
            inc = 156_250;
        }

        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            nominal_frequency: inc, 
        })
    }
// ... (enable_privilege) ...
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, ppm: f64) -> Result<()> {
        let adj_delta = (self.nominal_frequency as f64 * ppm / 1_000_000.0) as i32;
        let val = self.nominal_frequency as i32 + adj_delta;
        let new_adj = if val < 0 { 0 } else { val } as u32;

        unsafe {
            log::debug!("Adjusting frequency (Legacy): PPM={:.3}, Base={}, NewAdj={}", ppm, self.nominal_frequency, new_adj);
            
            // Legacy API: Enable adjustment
            if SetSystemTimeAdjustment(new_adj, false).is_ok() {
                Ok(())
            } else {
                let err = GetLastError();
                log::error!("SetSystemTimeAdjustment failed! Error: {:?}", err);
                Err(anyhow::anyhow!("SetSystemTimeAdjustment failed: {:?}", err))
            }
        }
    }
// ... (step_clock) ...
}