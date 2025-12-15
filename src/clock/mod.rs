use anyhow::Result;

pub trait SystemClock {
    /// Adjusts the system clock frequency.
    /// `factor`: The ratio of master speed to local speed.
    /// 1.0 means no adjustment.
    /// > 1.0 means local clock is too slow, speed up.
    /// < 1.0 means local clock is too fast, slow down.
    fn adjust_frequency(&mut self, factor: f64) -> Result<()>;
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::WindowsClock as PlatformClock;

#[cfg(unix)]
mod linux;
#[cfg(unix)]
pub use self::linux::LinuxClock as PlatformClock;
