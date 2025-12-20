use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, SYSTEMTIME, FILETIME};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustment, SetSystemTimeAdjustment,
    GetSystemTimeAsFileTime, SetSystemTime
};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::Duration;
use log::{info, warn, debug, error};

pub struct WindowsClock {
    original_adjustment: u32,
    original_increment: u32,
    original_disabled: BOOL,
    nominal_frequency: u32,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        let mut adj = 0u32;
        let mut inc = 0u32;
        let mut disabled = BOOL(0);

        unsafe {
            // Use the OLD API - it uses 100ns units and is more widely supported
            GetSystemTimeAdjustment(&mut adj, &mut inc, &mut disabled)?;
        }

        info!("Windows Clock Initial State (Old API): Adj={}, Inc={}, Disabled={}", adj, inc, disabled.as_bool());
        info!("  Inc in ms: {:.3}", inc as f64 / 10_000.0);

        // The increment is in 100ns units. Typical value is ~156,250 (15.625ms)
        let nominal = inc;
        info!("Using increment {} (100ns units) as nominal frequency", nominal);

        // If adjustment is currently disabled, enable it with the nominal value
        if disabled.as_bool() {
            info!("Enabling time adjustment with nominal={}", nominal);
            unsafe {
                SetSystemTimeAdjustment(nominal, false)?;
            }
            info!("Time adjustment enabled successfully.");
        }

        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            nominal_frequency: nominal,
        })
    }

    fn enable_privilege(name: &str) -> Result<()> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;
            
            let mut luid = LUID::default();
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name_wide.as_ptr()), &mut luid)?;
            
            let mut tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                ..Default::default()
            };
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

            AdjustTokenPrivileges(token, BOOL(0), Some(&tp), 0, None, None)?;
            
            if let Err(e) = GetLastError() {
                if e.code() == ERROR_NOT_ALL_ASSIGNED.to_hresult() {
                     return Err(anyhow!("Failed to adjust privilege: ERROR_NOT_ALL_ASSIGNED"));
                }
            }
            
            CloseHandle(token)?;
        }
        Ok(())
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        // factor is the ratio: 1.0 = no change, 1.001 = speed up by 1000 ppm
        // Adjustment = Nominal * factor (in 100ns units)
        // OLD API: dwTimeAdjustment = number of 100ns units added per lpTimeIncrement period
        let new_adj = (self.nominal_frequency as f64 * factor).round() as u32;

        // Convert factor to PPM for logging
        let ppm = (factor - 1.0) * 1_000_000.0;

        unsafe {
            debug!("Adjusting frequency (Old API): Factor={:.9}, PPM={:.3}, Base={}, NewAdj={}",
                   factor, ppm, self.nominal_frequency, new_adj);

            SetSystemTimeAdjustment(new_adj, false)?;

            // Verify the adjustment was actually applied
            let mut verify_adj = 0u32;
            let mut verify_inc = 0u32;
            let mut verify_disabled = BOOL(0);
            if GetSystemTimeAdjustment(&mut verify_adj, &mut verify_inc, &mut verify_disabled).is_ok() {
                if verify_adj != new_adj {
                    warn!("Adjustment mismatch! Requested={}, Actual={}", new_adj, verify_adj);
                }
            }
        }
        Ok(())
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        unsafe {
            let ft: FILETIME = GetSystemTimeAsFileTime();
            
            let mut u64_time = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);
            let offset_100ns = offset.as_nanos() as u64 / 100;
            
            if sign > 0 {
                u64_time += offset_100ns;
            } else {
                if u64_time > offset_100ns {
                    u64_time -= offset_100ns;
                } else {
                    return Err(anyhow!("Clock step would result in negative time"));
                }
            }
            
            let ft_new = FILETIME {
                dwLowDateTime: (u64_time & 0xFFFFFFFF) as u32,
                dwHighDateTime: (u64_time >> 32) as u32,
            };
            
            let mut st = SYSTEMTIME::default();
            if let Err(e) = FileTimeToSystemTime(&ft_new, &mut st) {
                 return Err(anyhow!("FileTimeToSystemTime failed: {}", e));
            }
            if let Err(e) = SetSystemTime(&st) {
                 return Err(anyhow!("SetSystemTime failed: {}", e));
            }
        }

        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        unsafe {
            // On exit, set clock to run at 1x speed using the nominal frequency
            // This ensures the system clock runs correctly even after the service stops
            let _ = SetSystemTimeAdjustment(self.nominal_frequency, false);
        }
    }
}