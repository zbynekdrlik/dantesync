//! The bench's world primitives: the seeded RNG, the ns-exact clock, the UTC reading noise.

use super::*;

/// #119 follow-up — how the master's UTC reading is disturbed.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum NtpNoise {
    /// A WAN upstream: σ = [`NTP_NOISE_NS`].
    Gauss,
    /// A mobile-data upstream: half the readings delayed by up to +5 ms one way, the other half
    /// early by up to 1.5 ms, on top of the WAN noise.
    Asymmetric5ms,
}

impl NtpNoise {
    pub(super) fn sample(self, rng: &mut Rng) -> i64 {
        let base = rng.gauss() * NTP_NOISE_NS;
        let jitter = match self {
            NtpNoise::Gauss => 0.0,
            NtpNoise::Asymmetric5ms if rng.uniform() < 0.5 => rng.uniform() * 5_000_000.0,
            NtpNoise::Asymmetric5ms => -rng.uniform() * 1_500_000.0,
        };
        (base + jitter).round() as i64
    }
}

/// xorshift64* — deterministic, dependency-free.
pub(super) struct Rng(pub(super) u64);
impl Rng {
    pub(super) fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub(super) fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub(super) fn gauss(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// A clock whose reading is an integer ns + a fractional carry (a wall of 1.79e18 ns does not fit
/// an f64 at ns resolution).
#[derive(Clone, Copy)]
pub(super) struct Clock {
    pub(super) ns: i64,
    pub(super) frac: f64,
}
impl Clock {
    pub(super) fn advance(&mut self, true_dt_ns: f64, rate_ppm: f64) {
        let d = true_dt_ns * (1.0 + rate_ppm * 1e-6) + self.frac;
        let whole = d.floor();
        self.frac = d - whole;
        self.ns += whole as i64;
    }
}

/// What each box hears: the grandmaster's UUID and its time base. The grandmaster CHANGES (to
/// another device: another UUID, uptime and oscillator) at `GM_CHANGE_AT_WINDOW`, and that new
/// grandmaster REBOOTS under the same UUID (its uptime restarts) at `GM_REBOOT_AT_WINDOW`. Each box
/// notices each event a few windows apart. At the change the MASTER is last, so followers
/// re-anchor while it still publishes a `D` in the old base (refused by the anchor grandmaster in
/// the extension). At the reboot the master is FIRST, so it publishes a `D` in the new base while
/// some followers are still in the old one under the SAME UUID: only the time-base check
/// (`same_time_base`) stops those from taking a multi-day "late" step.
pub(super) fn gm_view<'a>(
    w: u64,
    lags: (u64, u64),
    a: &'a Clock,
    b_pre: &'a Clock,
    b_post: &'a Clock,
) -> (u8, &'a Clock) {
    let (change_lag, reboot_lag) = lags;
    if w < GM_CHANGE_AT_WINDOW + change_lag {
        (1, a)
    } else if w < GM_REBOOT_AT_WINDOW + reboot_lag {
        (2, b_pre)
    } else {
        (2, b_post)
    }
}
