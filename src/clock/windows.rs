use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED, TOKEN_PRIVILEGES_ATTRIBUTES
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise,
};
use windows::core::PCWSTR;

pub struct WindowsClock {
    original_adjustment: u64,
    original_increment: u64,
    original_disabled: BOOL,
    nominal_frequency: u64,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled)?;
        }

        // Nominal frequency is usually what the system uses when not adjusted.
        // However, we need a baseline. The C++ code used 10,000,000.
        // If 'disabled' is TRUE, the current 'adj' might be meaningless or default.
        // If 'disabled' is FALSE, 'adj' is the current running value.
        
        // We'll use 10,000,000 (1 second in 100ns units) as our baseline 
        // assuming the increment interrupt is targeting that.
        // Actually, SetSystemTimeAdjustmentPrecise expects the value to be added 
        // *every time increment*.
        // If the time increment is, say, 15.625ms (156,250 units), then 
        // to keep time 1:1, we should add 156,250 units.
        
        // Wait, the C++ code uses 10,000,000 as "FIXED_NOMINAL_1_SECOND".
        // And sets that as the adjustment.
        // If the interrupt period is 15.6ms, adding 1 second every 15.6ms 
        // would make the clock run 64x fast.
        // This implies the C++ code assumes the interrupt period IS 1 second?
        // OR, the "adjustment" value is NOT "per interrupt" but "per second"?
        
        // Docs for SetSystemTimeAdjustment:
        // "dwTimeAdjustment: The number of 100-nanosecond units added to the time-of-day clock 
        // for each lpTimeIncrement period of time that actually passes."
        
        // So if increment is 15.6ms, we should add 15.6ms.
        // If we add 10,000,000 (1s), we fly.
        
        // UNLESS the 'increment' is 1 second? No, that's huge latency.
        
        // Maybe the C++ code is using `SetSystemTimeAdjustmentPrecise` differently or I misunderstand the constant.
        // Let's look at C++ code again:
        // `GetSystemTimeAdjustmentPrecise(&original_adjustment_, &original_interval_, ...)`
        // `original_interval_` (increment) is retrieved.
        // `FIXED_NOMINAL_1_SECOND` is defined as 10,000,000.
        // `SetSystemTimeAdjustmentPrecise(FIXED_NOMINAL_1_SECOND, TRUE)`
        
        // If `TRUE` is passed (disabled), the value 10,000,000 is IGNORED.
        // So the C++ code was likely just resetting it to system default.
        // AND THEN later:
        // `SetSystemTimeAdjustmentPrecise(new_adjustment_value, TRUE)`
        // It keeps passing TRUE. 
        // This implies the C++ code DOES NOT WORK for adjustment, 
        // it just calculates the value and then disables adjustment (effectively doing nothing).
        // That explains why they might not have noticed the "1 second per interrupt" issue if it was never enabled.
        
        // BUT, if I am to "improve it", I must make it work.
        // To make it work, I need to know the `dwTimeIncrement`.
        // I should use `original_increment` as the base.
        // If I want to speed up by 1%, I set `dwTimeAdjustment = original_increment * 1.01`.
        
        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            nominal_frequency: inc, // Use the system reported increment as nominal
        })
    }

    fn enable_privilege(name: &str) -> Result<()> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;
            
            let mut luid = LUID::default();
            let mut name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name_wide.as_ptr()), &mut luid)?;
            
            let mut tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                ..Default::default()
            };
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

            AdjustTokenPrivileges(token, BOOL(0), Some(&tp), 0, None, None)?;
            if GetLastError().is_err() {
                 // AdjustTokenPrivileges returns success even if it failed to adjust, 
                 // we must check GetLastError. 
                 // Wait, windows crate usually handles this via `?` if it returns BOOL?
                 // AdjustTokenPrivileges returns BOOL. 
                 // If it returns TRUE, we still check GetLastError for ERROR_NOT_ALL_ASSIGNED.
                 let err = GetLastError();
                 if err.0 != 0 {
                     return Err(anyhow!("Failed to adjust privilege: {:?}", err));
                 }
            }
            CloseHandle(token);
        }
        Ok(())
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let new_adj = (self.nominal_frequency as f64 * factor).round() as u64;
        
        // We MUST pass FALSE to enable adjustment.
        unsafe {
            SetSystemTimeAdjustmentPrecise(new_adj, BOOL(0))?;
        }
        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        unsafe {
            let _ = SetSystemTimeAdjustmentPrecise(self.original_adjustment, self.original_disabled);
        }
    }
}
