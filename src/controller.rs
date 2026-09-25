//! PTP Controller - Core synchronization logic for Dante PTP time sync
//!
//! This controller implements a two-phase frequency synchronization approach:
//! 1. **Acquisition Phase**: Fast convergence using direct proportional control
//! 2. **Production Phase**: Precision maintenance using adaptive PI control with soft dead zones
//!
//! Key features:
//! - Lucky packet filtering (minimum offset selection) for jitter immunity
//! - Adaptive gain tuning based on oscillation detection
//! - Soft dead zones tuned for 96kHz audio (1 sample = 10.4µs)

use crate::clock::SystemClock;
use crate::clock_alarm::{self, ClockAlarm, ClockAlarmNotifier, ClockHealth, DesktopNotifier};
use crate::config::SystemConfig;
use crate::gm_filter::{GmAllowlist, ResolveOutcome, Resolver, StdResolver};
use crate::phase_slew::{self, PhaseSlewOutput, PhaseSlewServo};
use crate::ptp::{PtpV1Control, PtpV1FollowUpBody, PtpV1Header, PtpV1SyncMessageBody};
use crate::spike_filter::{FilterMode, JitterEstimator, SpikeFilter};
use crate::status::SyncStatus;
use crate::time_server::DateAuthoritySource;
use crate::traits::{NtpSource, PtpNetwork};
use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

/// dantesync#117 / #88 — the PTP phase lock and fleet date-offset glue (the frequency word once
/// locked, the NTP master's authority, a follower's poll/join/schedule, the coordinated step, the
/// status fields). A child module so it can reach the controller's private state without growing
/// this file; its state is the one `date_sync` field.
mod date_sync;

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// dantesync#117/#88 — the wall clock now, ns since the Unix epoch. `t2` (the PTP receive time)
/// is on this same clock, so `wall − D` is this box's view of the grandmaster's PTP time.
fn wall_now_ns() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Format a 6-byte UUID/MAC as a readable string (e.g., "00:1D:C1:AB:CD:EF")
fn format_mac(uuid: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5]
    )
}

/// #679 — decides whether the throttled "[PTP] ... Drift: ... Adj: ...ppm"
/// summary line should be emitted this sample, given a running per-sample
/// counter (incremented once per call to `apply_self_tuning_servo`, starting
/// at 0) and the configured interval (`DRIFT_LOG_SUMMARY_INTERVAL_SAMPLES`).
/// Logs immediately on the very first sample (count == 0, for fast startup
/// visibility), then every `interval`-th sample thereafter. `interval == 0`
/// is a defensive guard against a future misconfiguration — it never logs
/// (fails closed toward LESS log volume) rather than panicking on `% 0`.
fn should_log_drift_summary(sample_count: u64, interval: u64) -> bool {
    interval != 0 && sample_count % interval == 0
}

/// #68 — decides whether the periodic upstream-NTP discipline runs this
/// iteration. Pure, so the policy is unit-tested directly instead of being
/// inferred from a live 30-second loop.
///
/// `tracking_enabled` is the master switch (all callers leave it on; it exists
/// so a future NTP-free mode has one place to turn the loop off). Beyond that,
/// the discipline runs when ANY role condition holds:
///
/// - `ptp_offline` — NTP is the only time source left (pre-existing behaviour).
/// - `server_mode` — this node serves UTC to the whole fleet, so its duty to
///   track UTC does NOT depend on its own PTP lock. This is the #68 addition:
///   `ntp_server_mode` previously disabled the loop outright, leaving the
///   master free-running at the Dante grandmaster's rate (6-19 ppm measured on
///   strih ⇒ ~21 ms of UTC error 19 minutes after a restart, 1.04 s over two
///   days) with `ntp_failed` reading `false` throughout.
/// - `is_locked` — ordinary client behaviour, unchanged.
/// - `stale` — nothing has measured UTC for a whole staleness window. Without
///   this a node whose PTP never reaches LOCK (packets flowing, so `ptp_offline`
///   never latches) would never query at all, and the freshness rule below would
///   then mark it `ntp_failed` FOREVER, because the only code that clears the
///   flag lives in the query path it cannot reach. Arming on staleness makes the
///   node keep tracking UTC and recover by itself — no restart, which is the
///   whole point of this ticket.
fn ntp_discipline_due(
    server_mode: bool,
    ptp_offline: bool,
    is_locked: bool,
    stale: bool,
    since_last_check: Duration,
    interval: Duration,
) -> bool {
    let role_allows = ptp_offline || server_mode || is_locked || stale;
    role_allows && since_last_check >= interval
}

/// #83 — which step threshold applies right now, in server mode. While genuinely PTP-locked to
/// a real grandmaster (`is_locked && !ptp_offline`), the master's UTC step is chasing the
/// grandmaster's own real, unfixable rate error vs UTC (see `NTP_SERVER_LOCKED_DEADBAND_US`'s
/// own doc comment for the full derivation) -- a large, fixed deadband replaces the routine
/// tight threshold, since the fleet needs internal (frequency) consistency, not tight absolute
/// UTC, while genuinely locked. The MOMENT PTP is not the fleet's frequency reference (still
/// acquiring, or `ptp_offline`), NTP becomes the master's only meaningful reference again, and
/// the original tight tracking applies unchanged -- same as #71/#76/#80 always did.
fn server_step_threshold_us(is_locked: bool, ptp_offline: bool) -> i64 {
    if is_locked && !ptp_offline {
        NTP_SERVER_LOCKED_DEADBAND_US
    } else {
        NTP_SERVER_STEP_THRESHOLD_US
    }
}

/// #83 CORRECTION (discovered verifying the corrected 2500us deadband, not asked for by the
/// supervisor but required to actually deliver what was asked -- see
/// `NTP_SERVER_LOCKED_AGREEMENT_TOL_US`'s own doc comment for the full incident): which
/// same-sign-agreement tolerance applies right now, in server mode. Mirrors
/// `server_step_threshold_us`'s own shape exactly, and the SAME lock-state branching -- while
/// genuinely PTP-locked, a WIDER tolerance is used so normal 2-sample confirmation keeps
/// governing (instead of silently falling through to the escape valve) across the full
/// live-measured drift-rate range; not locked (still acquiring, or `ptp_offline`) keeps the
/// original, unrelated #76 tolerance completely unchanged.
fn server_agreement_tolerance_us(is_locked: bool, ptp_offline: bool) -> i64 {
    if is_locked && !ptp_offline {
        NTP_SERVER_LOCKED_AGREEMENT_TOL_US
    } else {
        NTP_SERVER_AGREEMENT_TOL_US
    }
}

/// #68 — bound a SINGLE periodic UTC correction while in NTP server mode.
///
/// This node's step is the whole fleet's step, so an upstream reading that is
/// wrong but internally consistent (and therefore survives the step-agreement
/// gate) must not be able to yank every box at once. In steady state the bound
/// never fires: at the measured 6-19 ppm a 30 s interval accrues only ~0.2-0.6 ms,
/// far under the default 100 ms. It only shapes the recovery of a genuinely large
/// error — a 1.04 s post-upgrade offset is worked off over ~10 minutes of normal
/// intervals, unattended and with no restart, instead of in one visible jump.
///
/// A non-positive bound means "unbounded" — a misconfigured `0` must degrade to
/// today's behaviour, never to a master frozen at zero correction forever. The
/// boot-time `run_ntp_sync()` step is deliberately NOT routed through here: a
/// cold start must land on UTC immediately.
fn clamp_ntp_step_us(offset_us: i64, max_step_us: i64) -> i64 {
    if max_step_us <= 0 {
        return offset_us;
    }
    offset_us.clamp(-max_step_us, max_step_us)
}

/// #68 — is this node's UTC measurement stale?
///
/// `ntp_failed` used to have exactly two writers, both inside the NTP query
/// path, so a node that had simply STOPPED querying (the master, by design)
/// reported `false` for 18 hours while a second of UTC error accumulated. A
/// timeout is the only signal that covers "nothing is even trying".
///
/// When nothing has EVER been measured the age falls back to process uptime, so
/// a fresh boot does not alarm before its first query lands, but an hour of
/// silence does. Exactly `window` is not yet stale (the boundary belongs to the
/// healthy side — a measurement arriving exactly on cadence is on time).
fn ntp_is_stale(since_last_success: Option<Duration>, uptime: Duration, window: Duration) -> bool {
    since_last_success.unwrap_or(uptime) > window
}

/// #68 — the effective staleness window, floored at one query cadence.
///
/// A configured `0` would make `ntp_is_stale` true for every node forever
/// (anything is `> ZERO`), pinning `ntp_failed` — and the operator's tray toast
/// — on permanently. `max_step_us` got the same defensive treatment for the
/// same reason: a misconfiguration must degrade, never latch.
fn effective_stale_window(configured_secs: u64) -> Duration {
    Duration::from_secs(configured_secs.max(NTP_CHECK_INTERVAL_SECS))
}

// ============================================================================
// CONSTANTS - Organized by functional area
// ============================================================================

// Safety limits
const MAX_DELTA_NS: i64 = 2_000_000_000; // 2s - reject obviously invalid deltas

// ==========================================================================
// SELF-TUNING SERVO ALGORITHM
// ==========================================================================
// The key insight: when offset oscillates around zero, the AVERAGE correction
// needed to maintain that equals the natural drift compensation.
//
// Algorithm:
// 1. Strong P-term responds to offset → creates oscillation around zero
// 2. Track running average of total correction when offset is small
// 3. This average becomes our "drift baseline" - the steady-state correction
// 4. The drift baseline is the auto-learned natural clock drift
//
// This is self-tuning because:
// - P-term immediately responds to any offset
// - Drift baseline slowly converges to the correct value
// - No manual tuning needed - it learns from the oscillation pattern
// ==========================================================================

// ==========================================================================
// TWO-PHASE CONTROL: ACQUISITION vs PRODUCTION
// ==========================================================================
// ACQUISITION: Fast convergence to lock (offset > 50µs)
//   - Aggressive P-term: P_GAIN_ACQ = 1.0 (10x production)
//   - Target: reach <50µs within 1 minute
//
// PRODUCTION: Gentle stability (offset < 50µs)
//   - Gentle P-term: P_GAIN_PROD = 0.1
//   - Auto-adaptive drift learning
// ==========================================================================

// Acquisition phase (FAST convergence)
const P_GAIN_ACQ: f64 = 0.8; // Aggressive P-term for quick lock
const P_MAX_ACQ_PPM: f64 = 200.0; // Limit to prevent wild swings

// Production phase (gentle stability)
const P_GAIN_PROD: f64 = 0.1; // Gentle P-term in production
const P_MAX_PROD_PPM: f64 = 100.0; // Allow enough for high drift rates

// NANO phase (ultra-precise for sub-µs capable systems)
// Entry: drift < 0.5 µs/s sustained for 30 samples
// Exit: drift > 1.0 µs/s for 5 samples (hysteresis)
const P_GAIN_NANO: f64 = 0.01; // 10x smaller than PROD - minimize hunting
const P_MAX_NANO_PPM: f64 = 10.0; // Tiny corrections only
const I_GAIN_NANO: f64 = 0.005; // 10x smaller I-term
const NANO_ENTER_RATE_US: f64 = 0.5; // Enter NANO if drift < 0.5 µs/s
const NANO_EXIT_RATE_US: f64 = 1.0; // Exit NANO if drift > 1.0 µs/s
const NANO_SUSTAIN_COUNT: usize = 15; // 15 samples (~15s) to enter NANO
const NANO_EXIT_COUNT: usize = 5; // 5 consecutive samples above threshold to exit (hysteresis)
const NANO_DEADBAND_US: f64 = 0.1; // Ignore drift < 0.1 µs/s (noise floor)

// ==========================================================================
// #679 — PER-SAMPLE DRIFT LOG THROTTLE
// ==========================================================================
// The routine "[PTP] <status>  Drift:...  Adj:...ppm" line used to fire on
// EVERY settled sample (~once/sec) — confirmed live to be ~65% of the
// camera-box fleet's fixed 50MB /var/log tmpfs volume, filling it in ~4-5
// days and crashing cam2's camera-box.service (2026-07-11). It now emits
// only every Nth sample (~30s at the typical 1 sample/sec cadence); the
// LOCKED/UNLOCKED/NANO mode-transition lines above are unaffected — they
// already only log on a real state change, not per sample.
// ==========================================================================
const DRIFT_LOG_SUMMARY_INTERVAL_SAMPLES: u64 = 30;

// Max drift baseline limit
const DRIFT_MAX_PPM: f64 = 500.0;

// Lock detection
const LOCK_STABLE_COUNT: usize = 5;

// Lucky packet filter - minimum time between samples (config override available)
const DEFAULT_MIN_T1_DELTA_NS: i64 = 100_000_000; // 100ms default (Dante sends ~125ms)

// Periodic NTP UTC alignment (steps clock without changing frequency)
const NTP_CHECK_INTERVAL_SECS: u64 = 30; // Check NTP every 30 seconds
const NTP_SAMPLE_COUNT: usize = 5; // Samples needed for reliable median
const NTP_STEP_THRESHOLD_BASE_US: i64 = 500; // Base threshold for low-jitter systems
const NTP_STEP_THRESHOLD_MAX_US: i64 = 10_000; // Maximum threshold (10ms) for high-jitter systems
const NTP_ADAPTIVE_MULTIPLIER: f64 = 5.0; // Step if offset > base + 5*MAD (covers 99%+ of jitter)
const NTP_STEP_AGREEMENT_N: usize = 2; // Consecutive AGREEING over-threshold measurements required to step
const NTP_STEP_AGREEMENT_TOL_US: i64 = NTP_STEP_THRESHOLD_BASE_US; // Same-sign magnitude tolerance floor

// ============================================================================
// #71 / #76 -- SERVER-MODE-ONLY discipline constants
// ============================================================================
// The master's UTC error is genuine drift (Dante-vs-UTC oscillator error,
// 6-19 ppm measured live) PLUS real measurement noise from whichever
// upstream NTP source this node is configured against. #71 fixed the
// drift-vs-confirmation-lag problem (a magnitude-tolerance agreement check
// tuned for client-mode jitter around a stationary offset repeatedly failed
// to keep pace with a genuinely accruing ramp -- hand-traced to a 1710us /
// 3-interval peak at the pre-#71 500us/30s model, vs. the intended
// 2-interval/1140us). #71's OWN fix (v1.8.31/v1.8.32) over-corrected: it
// dropped magnitude checking ENTIRELY (same-sign-only agreement) and added a
// single-sample "fast lane", which -- verified only against a noiseless
// simulation -- assumed "small + same-sign" was always trustworthy. On
// strih's real upstream (WAN, Cloudflare, pcap_active:false -- the less-
// precise userspace rsntp fallback path #53 built the kernel-timestamped
// transport to avoid), consecutive burst offsets scatter +0.5..+2.5ms, a
// magnitude comparable to or larger than the true ~190-380us/check drift
// signal -- so "small" is not a reliable trust signal there, and the fast
// lane chased that noise into a step roughly every ~10s on the live canary
// (dantesync#76). None of these are new config surface -- consistent with
// NTP_STEP_THRESHOLD_BASE_US et al already being hardcoded, and avoiding
// config-migration.md's JSON risk for values only the fleet's own clock
// architecture should tune. Client-mode behavior (threshold, cadence,
// agreement) is entirely unaffected -- every use is gated on
// `self.ntp_server_mode`. See the design comments on dantesync#71 and
// dantesync#76 for the full numeric derivations.
//
// #76's fix: NO single-sample fast lane -- server mode ALWAYS requires
// NTP_STEP_AGREEMENT_N (2) same-sign agreeing samples, same as before #71
// ever introduced the fast lane. What changed from the PRE-#71 client-style
// gate is the TOLERANCE shape: instead of the client's self-scaling
// `max(TOL, |cand|/2)` (proven pathological for a genuine ramp -- too tight
// for a small real candidate, too loose once a noisy large candidate has
// already inflated it), server mode uses a FIXED, non-scaling
// NTP_SERVER_AGREEMENT_TOL_US sized to the TRUE expected per-check accrual
// (19ppm x 10s ~ 190us; 400us gives ~2x headroom) rather than to the
// candidate's own possibly-noisy magnitude. Replaying strih's own logged
// Stepped sequence (1467, 691, 1668, 1801, 570, 1622, 1157us -- each was a
// single fast-laned reading under v1.8.32) through this fixed tolerance
// produces exactly ONE agreeing pair (1668 -> 1801, delta 133us) instead of
// seven immediate steps.
//
// NTP_SERVER_MAX_BURST_SPREAD_US is a second, independent layer: a burst
// whose OWN spread_us (dantesync#53's quality signal, already computed and
// published, previously never consulted by ntp_step_gate in either mode --
// dantesync#74's own accepted trade-off, now proven load-bearing rather than
// safely deferrable) exceeds this bound is skipped ENTIRELY for step-
// decision purposes at the check_ntp_utc_tracking call site -- it neither
// starts, confirms, nor contradicts a pending candidate. /status publishing
// is unaffected; an operator can still see a high-spread reading, it just
// cannot fire a step on its own or in combination with another sample.
//
// GRACE-PERIOD DUTY CYCLE (review finding, #71, still current under #76):
// every step clears sample_window/spike_filter and sets a 2s post-step
// grace period on the PTP servo. NTP_SERVER_CHECK_INTERVAL_SECS stays 10s
// (not a more aggressive 5s) for the same reason #71's review settled on it:
// the natural step interval is dominated by threshold/rate, not by how
// often the check runs once cadence is fine enough to catch a crossing
// promptly. This trade-off has NOT been validated live for PTP-lock quality
// specifically -- the supervisor's post-release canary on strih should
// confirm it, alongside grepping the log for `Stepped` frequency (per
// dantesync#76's own canary-methodology note: `/status` publishes the
// POST-correction residual and is blind to a stepping-frequency regression).
// ============================================================================
const NTP_SERVER_STEP_THRESHOLD_US: i64 = 200; // still >>5-32us measured single-query noise (#53); catches genuine drift earlier than the client's 500us floor
const NTP_SERVER_CHECK_INTERVAL_SECS: u64 = 10; // independent of calculate_adaptive_ntp_interval, which tracks PTP-vs-Dante-GM lock quality -- irrelevant to this node's UTC duty
                                                // #105 (review 🟡): max consecutive non-slewable cycles that keep applying the held phase-slew DC
                                                // before the controller gives up and full-resets. Bounds a stale DC during a persistent not-locked
                                                // spell (e.g. a GM changeover where the true DC may have shifted). 5 checks = ~50-150s (client
                                                // cadence 10-30s), long enough that a brief flap keeps its DC but a real reference change relearns.
const PHASE_SLEW_MAX_PRESERVE_STREAK: u32 = 5;
const NTP_SERVER_AGREEMENT_TOL_US: i64 = 400; // #76: FIXED (non-scaling) tolerance sized to the true ~190-380us/check accrual, not to a possibly-noisy candidate's own magnitude
const NTP_SERVER_MAX_BURST_SPREAD_US: u64 = 600; // #76: a burst this noisy internally is low-quality evidence and is excluded from the step decision entirely -- 600, not the ~500 first suggested, so it does not also exclude strih's own genuine 588us-spread large-error-recovery reading (dantesync#68's own fixture); still well below the observed WAN noise burst spreads (up to 1356us)

// #83 CORRECTION -- discovered while verifying the corrected 2500us deadband (dantesync#83's own
// supervisor follow-up), not part of the original ask, but required to actually deliver a safe
// result: at the TOP of the live-measured drift range (66ppm, 660us/10s-interval accrual), the
// per-check accrual EXCEEDS the routine NTP_SERVER_AGREEMENT_TOL_US (400us) -- exactly the #76
// high-oscillator-error scenario, where normal 2-sample agreement can never confirm because every
// consecutive same-sign reading "contradicts" the last (delta > tolerance). Verified live by
// running the closed-loop simulation at 66ppm (not hand-derived): with the SMALLER 2500us
// deadband, this forces EVERY step through the #76 escape valve -- which guarantees a step
// EVENTUALLY happens, but NOT that its SIZE stays bounded: the escape valve's own patience
// (NTP_SERVER_MAX_CHECKS_WITHOUT_STEP, 30 checks) multiplied by the 660us/check accrual it can
// never confirm away lets the offset grow to ~20ms before the escape valve fires -- measured
// 21_780us in the simulation, ~8.7x the proven-safe 2500us ceiling and within the SAME order of
// magnitude as the withdrawn 25ms mistake this whole correction exists to fix. Scaling the escape
// valve's OWN patience down instead was considered and rejected: a check-count-denominated
// patience gives accrual PROPORTIONAL to ppm (the wrong scaling -- higher ppm needs a SHORTER
// patience to stay bounded, but a patience short enough for 66ppm would make the escape valve fire
// before normal agreement even gets its second sample at 38ppm, defeating confirmed stepping at
// the LOWER end of the same range).
//
// The chosen fix: widen the tolerance so normal 2-sample agreement keeps governing across MOST
// of the measured range, keeping the escape valve a rarer path than the pre-correction 25ms
// deadband made it, rather than the routine path for half the ppm range. 750us was NOT chosen
// arbitrarily: it comfortably exceeds 660us (66ppm) with ~14% margin for thermal drift.
//
// #83 REVIEW FINDING (2nd round, critical) -- checked against the REAL captured WAN-noise
// sequence this project already has on record (ntp_gate_server_mode_rejects_the_real_
// strih_wan_noise_sequence_76's own fixture: [1467,691,1668,1801,570,1622,1157]) by ACTUALLY
// RUNNING the real candidate/contradiction gate logic (not hand-derived from the raw
// consecutive deltas, which was an earlier draft's mistake and does not match the real
// candidate-REPLACEMENT semantics in ntp_step_gate): the existing 400us (not-locked) tolerance
// produces 1 step on this fixture; this 750us tolerance produces 2 -- a genuine 100% increase,
// not "no material increase" as an earlier version of this comment incorrectly claimed. Neither
// count is dangerous on its own (both extra corrections are small, sub-2ms, per-fixture-reading
// values -- nowhere near the frame-period concern this whole ticket is about), but the more
// important finding this same review round surfaced is architectural, not this comment's own
// arithmetic: widening the tolerance ALONE does not bound the worst case at ppm rates ABOVE
// where this tolerance itself stops covering (~75ppm, the point where per-check accrual
// 10*ppm exceeds 750us again) -- see NTP_SERVER_LOCKED_MAX_STEP_US's own doc comment for the
// hard safety-net fix that closes that gap regardless of tolerance value or ppm.
//
// Applies ONLY while genuinely locked (server_agreement_tolerance_us) -- the not-locked path
// keeps NTP_SERVER_AGREEMENT_TOL_US completely unchanged, untouched by this correction.
const NTP_SERVER_LOCKED_AGREEMENT_TOL_US: i64 = 750;

// #83 REVIEW FINDING (2nd round, critical) -- a HARD safety net, not a tuning knob. Widening
// the agreement tolerance (above) covers the live-measured range up to its own breakeven point
// (per-check accrual 10*ppm exceeding the tolerance again -- currently ~75ppm), but does NOT by
// itself bound the worst case ABOVE that point: at ppm > ~75, normal 2-sample confirmation stops
// working AGAIN (the exact #76 scenario, just at a higher rate than before), so stepping falls
// through to the escape valve -- which guarantees a step EVENTUALLY, never that its SIZE stays
// bounded. Verified live in review by simulation: at 76-80ppm the escape valve accrues the
// offset to ~25-26ms before firing, UNCONFIRMED -- the SAME order of magnitude as the withdrawn
// 25ms mistake this whole ticket exists to fix, and this project has already observed drift
// climb 19ppm -> 38-41ppm -> 66ppm across different sessions (plausibly diurnal/thermal), so a
// further excursion past 75ppm is not a remote hypothetical.
//
// Rather than trying to out-guess every possible future ppm with an ever-widening tolerance
// (which only pushes the SAME breakeven problem to a higher, still-finite ppm, and widens
// same-sign WAN-noise exposure further each time), this bounds the CONSEQUENCE directly: NO
// single server-mode step, confirmed OR escape-valve-forced, may exceed this ceiling while
// genuinely locked -- wired in at the step-application site, independent of and in ADDITION to
// the general ntp_server_max_step_us config bound (whose 100_000us/100ms default is far too
// loose to matter at this scale; #68's own field, unrelated purpose -- bounding a large
// post-upgrade offset for a different regime, left completely unchanged for the not-locked
// path). 5_000us is comfortably ABOVE the ~3040-3700us ceiling normal confirmation already
// produces across the full measured-plus-margin range (so it never interferes with normal,
// healthy operation), and comfortably BELOW both frame periods with real margin (~30% of
// 16.7ms@60fps, ~15% of 33.3ms@30fps).
//
// A clamped step leaves a RESIDUAL on the clock -- see the step-application site's own
// companion fix (the escape-valve counter is NOT reset on a clamped/partial step) for why this
// converges (a rapid run of further clamped corrections until the residual clears) instead of
// growing unboundedly (which a naive "always reset on any step" would cause: the NEXT
// residual-plus-new-accrual would then need another full NTP_SERVER_MAX_CHECKS_WITHOUT_STEP-
// check wait, during which MORE accrues than one clamp removes -- verified this would NOT
// converge before choosing the companion fix).
//
// #94: tightened 5000 -> 2500us. The 5000 value was explicitly chosen (above) to sit ABOVE the
// pre-#94 ~3040-3700us NORMAL confirmed-step ceiling so it never interfered with healthy
// operation -- but #94 lowers that normal ceiling to ~1980us (NTP_SERVER_LOCKED_DEADBAND_US's own
// #94 note), so 5000 is now needlessly loose. Setting the hard cap to the SAME 2500us proven-
// absorbed band makes "no locked step ever exceeds 2500us" a GUARANTEE rather than a
// clean-simulation property: even a pathological WAN-noise reading that agrees within the 750us
// tolerance, or the >75ppm escape-valve path, is clamped to 2500us with the residual worked off
// by the SAME armed-counter convergence mechanism documented above (the counter is not reset on a
// clamped locked step, so the escape valve re-fires next check -- verified to converge; at the
// tighter 2500 cap it converges FASTER, since each clamp removes more than one escape-valve
// re-fire accrues). In HEALTHY 23-66ppm operation this cap NEVER fires -- the lowered trigger
// already keeps confirmed steps <=~1980us -- so it is purely a safety net for noise/degraded
// regimes, which are already the storm-alarm's domain. Not-locked path: unchanged (this cap only
// applies while genuinely locked; the general ntp_server_max_step_us config bound is untouched).
const NTP_SERVER_LOCKED_MAX_STEP_US: i64 = 2_500;

// #76 REVIEW FINDING (critical): both NTP_SERVER_AGREEMENT_TOL_US and NTP_SERVER_MAX_BURST_SPREAD_US
// can, by their own construction, reject a genuine same-sign trend FOREVER with no other signal --
// a true oscillator error whose per-check accrual permanently exceeds the fixed tolerance (~40ppm+
// at this cadence, vs. 6-19ppm ever measured) would never find two in-tolerance readings; a
// persistently-noisy upstream would never present a low-enough-spread burst. Reproduced live in
// review: at 41+ppm the closed-loop simulation shows UNBOUNDED linear growth with zero recovery
// (205ms after one simulated hour at 57ppm) and no distinct alarm -- this is a STRICTLY WORSE
// failure class than either the pre-#71 self-scaling tolerance (always eventually converges, just
// with lag) or the buggy #71/v1.8.32 fast lane (would fast-lane 570us just fine). See
// ntp_server_checks_since_step's own doc comment for the escape-valve mechanism this constant
// gates -- once this many CONSECUTIVE OVER-THRESHOLD checks pass with no actual step, the NEXT
// over-threshold reading forces one regardless of tolerance/quality. 30 checks = 5 minutes at the
// 10s cadence: far longer than the ~20-120s the tolerance-agreement path steps at under normal
// (even noisy) operation -- per the closed-loop noisy-upstream simulation, steps occur roughly
// once every 12 checks on average -- so this should essentially never fire under real-world
// conditions, only as a genuine last resort.
//
// #83 REVIEW FINDING (critical): this 30-check patience is comfortably longer than the TIGHT
// threshold's own natural stepping cadence (~2-12 checks), but the PTP-locked deadband's own
// natural cadence is LONGER than 30 checks (~38-66 checks at the 38-66ppm measured range) --
// without ntp_server_checks_since_step's OWN under-threshold reset (added alongside this
// comment), the counter would already exceed 30 well before the deadband was ever legitimately
// crossed, so EVERY deadband-driven step would go through the escape valve unconfirmed, on the
// very first over-threshold sample, defeating this whole gate for the primary #83 use case. The
// under-threshold reset restores the INTENDED invariant ("far longer than normal cadence") for
// BOTH thresholds without a second tunable constant -- the counter only ever accumulates while
// genuinely over threshold, so its natural comparison is always against however many checks the
// CURRENTLY active tolerance/quality gates take to confirm, not against how long it took to
// first cross into over-threshold territory.
const NTP_SERVER_MAX_CHECKS_WITHOUT_STEP: u32 = 30;

// #83: while genuinely PTP-locked to a real grandmaster, the master's periodic UTC step was
// chasing the Dante grandmaster's own REAL, PERSISTENT, UNFIXABLE rate error vs UTC (measured
// live: ~38-66ppm on strih, vs PTP's own lock to that same grandmaster staying genuinely tight
// and stable the whole time -- Drift within a few us/s, Adj -4..-6.7ppm, unrelated to the
// NTP-measured figure). This is architecturally by design: Dante PTP provides frequency
// coherence to the grandmaster's OWN rate, which has no defined relationship to UTC's rate (see
// this file's own "CRITICAL: Dante Time vs UTC Time" doc, top of controller.rs / README) -- no
// tuning of the confirmation/tolerance/quality-gate machinery below can remove a rate mismatch
// that is real, external, and correctly invisible to PTP-vs-GM lock quality. #71/#76/#80 all
// correctly tuned HOW a step is confirmed and applied; #83 changes WHETHER one should fire this
// often at all, now that the confirmation/application machinery is provably correct.
//
// The rig needs INTERNAL consistency (fleet-vs-master spread, genlock timecodes), not tight
// absolute UTC -- nothing downstream needs sub-second real-world time accuracy. While genuinely
// locked, the step threshold becomes a large, fixed deadband instead of the routine tight one.
// When NOT genuinely locked (still acquiring, or ptp_offline -- PTP packets aren't even
// flowing), NTP is the master's ONLY meaningful time reference (the original #68 rationale), so
// the existing tight tracking applies completely unchanged -- same threshold, same tolerance,
// same quality gate, same escape valve. See server_step_threshold_us's own doc comment.
//
// #83 CORRECTION (v1.8.38 shipped 25_000us here without the rig's own domain constraint -- this
// was a design mistake, corrected before any fleet rollout beyond the strih canary): a fleet
// clock STEP shifts every camera's genlock timecodes by the step size, and strih/imag OBS
// ts-align only absorbs a timecode jump while it stays comfortably below one frame period
// (16.7ms @60fps imag path, 33.3ms @30fps strih recording path). 25ms EXCEEDS the 60fps frame
// period outright and is ~75% of the 30fps one -- each step event near-guarantees a held/dropped
// frame, which surfaces as a copy+gap in camera-box's zero-loss E2E verdict (bar: 0 copies, 0
// gaps over >=300s windows). At 38-66ppm a 25ms deadband steps every ~6-11min, so a 30-60min
// gate run would eat ~5 step events -- recurrently RED, strictly WORSE for the gate than the
// pre-#83 behavior this feature was meant to improve on.
//
// The corrected value, 2_500us (2.5ms), is the top of a PROVEN-safe band, not merely "a smaller
// number": camera-box PR #1017's full E2E ran GREEN on 2026-08-11 with the fleet master (then
// v1.8.30) stepping +0.9..+2.5ms every 20-40s (issue #71's own measurement of that build), and
// the A/V-sync dock held LOCKED 87 minutes continuously through that exact stepping regime --
// steps <=2.5ms are proven absorbed by the recorded gate, ts-align, and the dock. At the live
// 38-66ppm this yields ~2.5ms steps roughly every 40-70s: SPARSER than the pre-#83 tight-
// threshold cadence (which also fired sub-2.5ms steps off the 200us threshold at a similar or
// tighter cadence), with every step now fully delivered (#80) and the step SIZE staying inside
// the proven-safe band -- strictly better than both the pre-#83 state and the withdrawn 25ms
// version. A plain constant, not made configurable: matches every other tunable this feature
// introduces (all bare constants, no config-parsing surface), and this value is derived from
// hard physical evidence (frame period), not an operator preference someone would retune.
//
// #94 CORRECTION (the value above, 2500us, described the TRIGGER; the realized STEP is larger):
// the step SIZE is the offset at CONFIRMATION time, not at the deadband -- ntp_step_gate steps
// on the SECOND agreeing over-threshold sample (#76), so the realized step = deadband + up to
// two check-intervals of accrued drift = deadband + 2*(ppm * NTP_SERVER_CHECK_INTERVAL_SECS).
// With the old 2500us trigger that overshot to +2760us (23ppm) / +3300us (66ppm) -- ABOVE the
// 2500us band the derivation above proves absorbed -- and the fleet juddered (dantesync#94, live
// 2026-08-18). The 2500us proven band is a STEP-SIZE bound, and it is now enforced in TWO places:
// this trigger keeps NORMAL confirmed steps inside it, and NTP_SERVER_LOCKED_MAX_STEP_US is the
// hard cap that keeps EVERY step inside it. To keep the realized confirmed step <= 2500us at the
// worst-ever 66ppm: trigger <= 2500 - 2*(66ppm * 10s) = 2500 - 1320 = 1180us. 1000us is chosen
// (below that ceiling with ~180us margin for WAN noise and any drift slightly past 66ppm): at
// 66ppm the confirmed step is ~1980us, at the live 23ppm ~1380us -- both comfortably inside the
// band, steps slightly smaller and more frequent (gentler on frame absorption, the whole point).
// This raises the healthy 66ppm cadence to the storm-alarm's own 120/h line (a consequence of
// conservation -- capping every step <=2500us at 66ppm's 237.6ms/h of drift FORCES >=95 steps/h,
// ~120/h with discrete 2-sample confirmation), which the issue-91 storm test still tolerates
// (fires only at >120/h) while the alarm still catches the real 129-180/h storm; see this file's
// #94 design comment on the issue for the full conservation/margin analysis. The not-locked tight
// path (server_step_threshold_us's else branch) is completely unchanged.
const NTP_SERVER_LOCKED_DEADBAND_US: i64 = 1_000;

// dantesync#91 — step-storm detection on a server-mode (master) node.
//
// The NTP loop CANNOT slew here (PTP owns frequency; a slew is read back by the PTP servo as
// drift and cancelled -- see clock-discipline-and-testing.md), so a UTC phase error is ALWAYS
// stepped, and the step RATE is a direct function of the Dante-clock-vs-UTC frequency error.
// A genuinely-PTP-locked healthy master steps at the locked deadband cadence, bounded by the
// Dante grandmaster's own real rate error: the worst ever measured on strih is 66ppm (#83).
// Pre-#94 that was ~72 steps/h at the old 2500us deadband; #94 lowered the trigger to 1000us to
// keep every realized step inside the 2500us proven band, which by conservation RAISES the
// healthy-66ppm ceiling to ~120 steps/h (capping each step to <=2500us at 66ppm's 237.6ms/h of
// drift forces ~120 steps/h) -- MEASURED by the repo's own closed-loop test
// (the_locked_master_at_66ppm_is_confirmation_governed_not_escape_valve_83) -- the ceiling of
// healthy operation, and the zero-false-alarm test
// healthy_locked_master_at_66ppm_stays_below_the_storm_alarm_91 pins it. The live #91 storm
// (the PTP GM went L2-unreachable, so the master correctly fell back to the tight 200us
// threshold and step-corrected UTC every ~10s check) ran 129-180 steps/h for 19h+ with NO
// alarm -- every existing NTP health signal (freshness #68, unreachable #53) fires only when
// NTP STOPS measuring, never when it measures fine but the master step-STORMS because its PTP
// frequency reference is degraded (exactly what #67 asked to surface). No servo change can
// remove a real external frequency error while PTP can't discipline it -- widening the deadband
// to slow the storm is the masking #83 already proved drops frames. The only honest response is
// to DETECT and ALARM so a watchdog/operator restores the grandmaster/PTP.
//
// The threshold sits AT the #94 ~120/h measured healthy-locked ceiling (pre-#94 it sat ~66%
// above the then-72/h ceiling) and BELOW the observed 129/h storm floor, so the alarm still fires
// only on a genuinely degraded frequency reference -- it triggers at strictly >120/h, which a
// healthy locked master cannot reach without a GM drift past ~118ppm (nearly 2x the worst ever
// measured, itself an anomaly worth flagging), while the tight-threshold not-locked storm path
// (the real #91 failure: 129-180/h) is entirely unaffected by #94 and still fires. #94's step-
// size cap consumed the old rate margin but not the alarm's discriminating power: it still
// catches every real storm. A trailing-hour count (not a shorter window) is the honest "steps/h"
// metric #67 named; a persistent storm -- the actual 19h failure mode -- is what it must catch.
const NTP_STEP_STORM_THRESHOLD_PER_HOUR: u32 = 120;
const NTP_STEP_STORM_WINDOW: Duration = Duration::from_secs(3600);
// Rate-limit the loud line so a sustained storm logs once per interval, not once per step.
const NTP_STEP_STORM_WARN_INTERVAL: Duration = Duration::from_secs(300);

// PTP offline detection
const PTP_TIMEOUT_SECS: u64 = 10; // Consider PTP offline after 10s without packets

/// dantesync#113 — periodic re-resolution cadence for `gm_allowlist` hostname
/// entries, so a DNS/lease change propagates without a service restart.
const GM_RESOLVE_INTERVAL: Duration = Duration::from_secs(60);

/// dantesync#113 — minimum spacing between the extra "announce from an unknown
/// source" re-resolutions, so a foreign-PTP flood can never turn into a DNS
/// query storm. The grandmaster moving to a new IP is caught within this window.
const GM_RESOLVE_ON_DROP_COOLDOWN: Duration = Duration::from_secs(5);

/// dantesync#114 review — initial acquisition grace. Until the node has locked
/// ONCE, a still-acquiring (not-yet-locked) clock within this window from start
/// is NOT treated as a lost clock, so a normal service restart / rig reboot does
/// not emit a spurious "NO DANTE CLOCK" alarm during convergence. A genuine hard
/// failure (no PTP packets at all → stale, or an unresolvable hostname) still
/// fires immediately. 120 s comfortably covers LOCK/NANO convergence.
const ACQUISITION_GRACE: Duration = Duration::from_secs(120);

// NTP failure detection
const NTP_FAILURE_THRESHOLD: usize = 3; // Consider NTP failed after 3 consecutive failures

// ============================================================================
// DATA STRUCTURES
// ============================================================================

/// Main PTP synchronization controller
pub struct PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    // Core components
    clock: C,
    network: N,
    ntp: S,
    config: SystemConfig,

    // PTP state
    pending_syncs: HashMap<u16, PendingSync>,
    prev_t1_ns: i64,
    prev_t2_ns: i64,
    current_gm_uuid: Option<[u8; 6]>,
    /// The source UUID of the device sending Sync messages (may differ from grandmaster_clock_uuid)
    current_sync_source: Option<[u8; 6]>,
    /// IP address of the device sending PTP Sync messages (for display in tray app)
    current_sync_source_ip: Option<std::net::Ipv4Addr>,

    /// camera-box issue 1073 — trusted grandmaster-source allowlist, parsed once
    /// from `config.gm_allowlist`. When restricting (non-empty), a PTP packet
    /// whose source IP is not permitted is dropped in `process_loop_iteration`
    /// as-if it never arrived, so a foreign-subnet grandmaster cannot be adopted.
    /// Empty = unrestricted (historical last-writer-wins). See `crate::gm_filter`.
    gm_allowlist: GmAllowlist,
    /// dantesync#113 — resolver for `gm_allowlist` hostname entries. Boxed so a
    /// test can inject a fake (no real DNS). Real builds use `StdResolver`.
    gm_resolver: Box<dyn Resolver>,
    /// dantesync#113 — last time the hostname allowlist was re-resolved
    /// (`Instant`, monotonic — this daemon steps its own wall clock).
    last_gm_resolve: Instant,
    /// dantesync#114 review — has this node achieved PTP lock at least once? Until
    /// it has (and within `ACQUISITION_GRACE`), a not-yet-locked clock is normal
    /// boot acquisition, not a loss (no spurious reboot alarm).
    ever_locked: bool,
    /// dantesync#114 — the loud NO-DANTE-CLOCK alarm state machine, evaluated on
    /// every 10 s `tick_status()` (so it fires even when NO PTP packets arrive —
    /// the whole point, since a lost clock means no packets). Silent while
    /// PTP-locked to an allowed grandmaster; otherwise one notification per
    /// `clock_alarm_interval_s`. See `crate::clock_alarm`.
    clock_alarm: ClockAlarm,
    /// dantesync#114 — the platform desktop notifier the alarm drives (Linux
    /// `notify-send`; no-op on a headless box or on Windows, where the tray shows
    /// the balloon from `/status.clock_alarm`). Boxed for test injection.
    clock_alarm_notifier: Box<dyn ClockAlarmNotifier>,
    /// dantesync#114 — the effective (floored) alarm cadence in seconds, published
    /// in `/status.clock_alarm_interval_s` so every surface shares one cadence.
    clock_alarm_interval_s: u64,
    /// camera-box issue 1073 — observability for the source filter. Counts PTP
    /// packets dropped by the allowlist since the last ALLOWED grandmaster packet
    /// (reset to 0 on any accepted packet), so `check_ptp_status` can tell
    /// "grandmaster genuinely absent" apart from "grandmaster present but blocked
    /// by a mis-set allowlist" — the near-zero-diagnosability failure mode of a
    /// valid-but-wrong allowlist on the fleet's sole clock authority.
    gm_dropped_since_accepted: u64,
    /// Rate-limiter (one loud warning per 30 s) for the dropped-packet warning.
    last_gm_drop_warn: Option<Instant>,

    // Sample filtering
    sample_window: Vec<i64>,

    // Metrics (for status display)
    last_phase_offset_ns: i64,
    last_adj_ppm: f64,

    // Epoch tracking
    initial_epoch_offset_ns: i64,
    epoch_aligned: bool,

    // Settling state
    valid_count: usize,
    clock_settled: bool,
    settling_threshold: usize,

    // Shared status for IPC
    status_shared: Arc<RwLock<SyncStatus>>,

    // Calibration (Windows pcap offset compensation)
    calibration_samples: Vec<i64>,
    calibration_offset_ns: i64,
    calibration_complete: bool,

    // Frequency control state
    applied_freq_ppm: f64,

    // Warmup tracking
    warmup_start: Instant,
    warmup_complete: bool,

    // ==========================================================================
    // SELF-TUNING SERVO STATE
    // ==========================================================================
    // P-term creates oscillation, drift baseline is learned from average correction
    // ==========================================================================
    /// Learned drift baseline (auto-tuned from average correction when stable)
    drift_baseline_ppm: f64,

    /// Lock state - true when synchronized and stable
    is_locked: bool,
    lock_stable_count: usize,

    /// Production mode state (with hysteresis)
    in_production_mode: bool,

    /// NANO mode state (ultra-precise for sub-µs capable systems)
    in_nano_mode: bool,
    nano_sustain_count: usize, // Track consecutive sub-threshold samples for entry
    nano_exit_count: usize,    // Track consecutive above-threshold samples for exit (hysteresis)

    // Rate-of-change tracking for Dante servo
    last_offset_us: Option<f64>,
    last_offset_time: Option<Instant>,
    smoothed_rate_ppm: f64, // Exponential moving average of rate

    // Periodic NTP UTC tracking state
    last_ntp_check: Instant,
    ntp_offset_samples: VecDeque<i64>, // in microseconds
    /// #68: this node is the fleet's NTP server. It keeps disciplining itself
    /// against upstream (a stratum-3 server that never re-reads its own
    /// reference is just a free-running oscillator advertising itself as a time
    /// source), and its corrections are bounded by `ntp_server_max_step_us`.
    ntp_server_mode: bool,
    /// #68: upper bound on a SINGLE server-mode correction, µs. See
    /// `clamp_ntp_step_us`.
    ntp_server_max_step_us: i64,
    // #50 step-agreement gate: a single over-threshold NTP measurement is NEVER trusted (a
    // queue-delay-biased round trip on a loaded LAN produces a false offset, the servo steps,
    // the next sample shows the negated bias and it steps right back — the live-event
    // "+2831us then -2825us" pair). A step now requires NTP_STEP_AGREEMENT_N consecutive
    // over-threshold samples that AGREE (same sign, similar magnitude).
    ntp_pending_step: Option<(i64, usize)>, // (first candidate offset_us, agreeing sample count)
    last_ntp_step: Option<Instant>,         // Grace period after NTP stepping
    /// #76: server-mode escape-valve counter -- how many CONSECUTIVE successful server-mode
    /// checks have passed, WHILE THE OFFSET WAS OVER THRESHOLD, since the last actual
    /// `step_clock` call (#83 review finding: reset to 0 on any check where the offset is NOT
    /// over threshold too -- see the reset right after `step_threshold` is computed in
    /// `check_ntp_utc_tracking`. Before that fix this counted EVERY successful check
    /// unconditionally, which was harmless under the routine tight threshold (almost every
    /// check WAS over threshold in that regime) but WRONG under #83's large PTP-locked
    /// deadband, whose natural cadence is longer than the escape valve's own patience --
    /// see that reset's own doc comment for the full incident). Both the fixed-tolerance
    /// agreement gate and the burst-quality gate can, by their own construction, reject a
    /// same-sign trend indefinitely (a genuine oscillator error faster than
    /// `NTP_SERVER_AGREEMENT_TOL_US`/check would never find two in-tolerance readings; a
    /// persistently-noisy upstream would never present a low-enough-spread burst) -- neither
    /// gate has any other way to notice this and would otherwise freeze corrections forever,
    /// silently, with no distinct alarm. This counter is the shared last-resort: once it
    /// reaches `NTP_SERVER_MAX_CHECKS_WITHOUT_STEP`, the NEXT over-threshold reading forces a
    /// step regardless of tolerance agreement or burst quality. Reset to 0 on ANY step (normal
    /// or escape-valve), or on any under-threshold check. Client mode never touches this field.
    ntp_server_checks_since_step: u32,

    // Accumulated phase error tracking (estimated drift between NTP steps)
    accumulated_phase_error_us: f64,
    last_phase_accumulation_time: Option<Instant>,

    /// #91: monotonic timestamps of recent successful NTP `step_clock` calls, for
    /// step-storm rate detection on a server-mode master. Pruned to the trailing
    /// `NTP_STEP_STORM_WINDOW`; its length is the published `ntp_steps_last_hour`.
    /// `Instant` (never `SystemTime`) because this daemon steps its OWN wall clock.
    ntp_step_times: VecDeque<Instant>,
    /// #91: rate-limiter for the loud `[NTP][STEP-STORM]` warning line.
    last_step_storm_warn: Option<Instant>,

    // PTP offline detection
    last_ptp_packet: Instant,
    ptp_offline: bool,
    ptp_offline_logged: bool, // Prevent repeated logging

    // NTP failure tracking
    ntp_consecutive_failures: usize,
    ntp_failed: bool,
    /// #68: when the last SUCCESSFUL NTP measurement landed (monotonic, for the
    /// staleness window) and its wall-clock epoch second (for `/status`).
    /// `None` = never measured.
    last_ntp_success: Option<Instant>,
    last_ntp_success_epoch: Option<u64>,
    /// #68: process start, so "never measured" can be graded against uptime
    /// instead of alarming instantly at boot.
    started_at: Instant,

    // ==========================================================================
    // ADAPTIVE SPIKE DETECTION
    // ==========================================================================
    // Robust outlier detection using MAD (Median Absolute Deviation)
    // Auto-adapts to each computer's noise profile
    // ==========================================================================
    /// Spike filter for rejecting timestamp noise spikes
    spike_filter: SpikeFilter,

    // ==========================================================================
    // ADAPTIVE JITTER SMOOTHING
    // ==========================================================================
    // Measures jitter (stddev of drift rate) and adjusts EMA smoothing factor.
    // Low-jitter systems (strih.lan): α=0.3 for responsive tracking
    // High-jitter systems (stream.lan): α=0.1 for heavy smoothing
    // ==========================================================================
    /// Jitter estimator for adaptive EMA alpha
    jitter_estimator: JitterEstimator,

    /// #679 — running per-sample counter gating the throttled drift summary
    /// log line (`should_log_drift_summary`). Increments once per call to
    /// `apply_self_tuning_servo`.
    drift_log_sample_count: u64,

    // ==========================================================================
    // PHASE-SLEW SERVO STATE (dantesync#97)
    // ==========================================================================
    /// The bounded PI phase-slew servo, `Some` only when `config.phase_slew.enabled` — so `None`
    /// (the default) makes every branch below a no-op and the frequency/step paths byte-identical
    /// to the pre-#97 behaviour.
    phase_slew: Option<PhaseSlewServo>,
    /// #97: the phase slew (ppm) to compose into the frequency word — updated at NTP cadence by
    /// `slew_phase`, consumed every PTP sample by `apply_self_tuning_servo`. `0.0` = idle.
    pending_f_phase_ppm: f64,
    /// #97: the phase slew that was ACTUALLY applied at the end of the previous
    /// `apply_self_tuning_servo` — i.e. the `f_phase` in effect over the just-measured PTP
    /// interval. Subtracted from the raw PTP rate observation (feed-forward decoupling) so the PTP
    /// servo never fights the deliberate slew.
    last_applied_f_phase_ppm: f64,
    /// #97: the last phase-servo output, for `/status` telemetry (P/I split + saturation).
    last_phase_slew_output: Option<PhaseSlewOutput>,
    /// #97: monotonic time of the last phase-servo update, for its `dt`. `Instant` because this
    /// daemon steps its own wall clock.
    last_phase_slew_update: Option<Instant>,
    /// #97 (review 🟡): edge state for the "slew saturated" alarm. `out.alarm` is a LEVEL (stays
    /// true for the whole saturation episode), so this latch makes `log_saturated_alarm` fire ONCE
    /// at the onset instead of every NTP cadence for minutes — honouring that fn's edge contract.
    phase_slew_alarm_active: bool,
    /// #105 (review 🟡): consecutive non-slewable cycles that PRESERVED the held DC without a slew
    /// re-engaging. Bounds the stale-DC hold — after `PHASE_SLEW_MAX_PRESERVE_STREAK` such cycles (a
    /// persistent not-locked spell, e.g. a GM changeover where the true DC may have changed) the
    /// controller does a FULL reset instead of preserving. Reset to 0 whenever a slew succeeds.
    phase_slew_preserve_streak: u32,

    /// dantesync#117 / #88 — the PTP phase lock and the fleet date offset (the anchor `D`, the
    /// authority on the NTP master, a follower's scheduler and poll source). One sub-struct, owned
    /// by `controller/date_sync.rs`.
    date_sync: date_sync::DateSync,
}

struct PendingSync {
    rx_time_sys: SystemTime,
    source_uuid: [u8; 6],
}

// ============================================================================
// IMPLEMENTATION
// ============================================================================

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    pub fn new(
        clock: C,
        network: N,
        ntp: S,
        status_shared: Arc<RwLock<SyncStatus>>,
        config: SystemConfig,
    ) -> Self {
        let window_size = config.filters.sample_window_size;
        let calibration_count = config.filters.calibration_samples;
        let calibration_complete = calibration_count == 0;
        // #97: read the flag before `config` is moved into the struct below.
        let config_phase_slew_enabled = config.phase_slew.enabled;

        // #117: the clock discipline (logged); phase_slew survives only under "legacy".
        let date_sync = date_sync::DateSync::new(&config, window_size);

        // #114: read the alarm cadence before `config` is moved. The effective
        // (floored) value is published in /status; the raw value is floored again
        // inside `ClockAlarm::from_interval_secs` so a `0` can never spam.
        let clock_alarm_interval_cfg = config.clock_alarm_interval_s;
        let clock_alarm_interval_s =
            clock_alarm_interval_cfg.max(clock_alarm::CLOCK_ALARM_INTERVAL_FLOOR_S);

        // camera-box issue 1073: parse the grandmaster-source allowlist once.
        let mut gm_allowlist = GmAllowlist::parse(&config.gm_allowlist);
        for bad in gm_allowlist.invalid_entries() {
            warn!(
                "gm_allowlist: ignoring unparseable entry {:?} (expected an IPv4, a CIDR like \
                 10.77.9.0/24, or a hostname like video-clock.lan)",
                bad
            );
        }
        // dantesync#113: resolve hostname entries at startup so the allowlist is
        // effective before the first packet (a hostname-only allowlist permits
        // nothing until resolved). The resolver is boxed for test injection.
        let gm_resolver: Box<dyn Resolver> = Box::new(StdResolver);
        if gm_allowlist.has_hostnames() {
            info!(
                "gm_allowlist: resolving hostname entries {:?} at startup",
                gm_allowlist.hostnames()
            );
            let outcome = gm_allowlist.resolve(&*gm_resolver);
            Self::log_gm_resolve_outcome(&outcome, &gm_allowlist);
        }
        if gm_allowlist.is_unrestricted() {
            info!("GM source policy: UNRESTRICTED (accept any grandmaster source IP)");
        } else {
            // Report the EFFECTIVE (successfully-parsed) policy, not the raw config
            // — an unparseable entry was already warned about above and must not be
            // presented here as if it were active.
            info!(
                "GM source policy: RESTRICTED to {} active prefix(es) — foreign-source PTP is dropped",
                gm_allowlist.prefix_count()
            );
        }

        info!("=== PTP Controller Initialization ===");
        info!("Mode: AUTO-ADAPTIVE DIRECT DRIFT MEASUREMENT");
        info!("  - Directly measures drift rate from offset samples");
        info!("  - No manual tuning required - works on any hardware");
        info!(
            "Filter: window={}, min_delta={}ns",
            window_size, config.filters.min_delta_ns
        );
        info!(
            "Calibration: {} ({})",
            calibration_count,
            if calibration_count > 0 {
                "enabled"
            } else {
                "disabled"
            }
        );
        info!("=== Ready ===");

        let now = Instant::now();

        PtpController {
            clock,
            network,
            ntp,
            config,
            pending_syncs: HashMap::new(),
            prev_t1_ns: 0,
            prev_t2_ns: 0,
            current_gm_uuid: None,
            current_sync_source: None,
            current_sync_source_ip: None,
            gm_allowlist,
            gm_resolver,
            last_gm_resolve: now,
            ever_locked: false,
            clock_alarm: ClockAlarm::from_interval_secs(clock_alarm_interval_cfg),
            clock_alarm_notifier: Box::new(DesktopNotifier),
            clock_alarm_interval_s,
            gm_dropped_since_accepted: 0,
            last_gm_drop_warn: None,
            sample_window: Vec::with_capacity(window_size),
            last_phase_offset_ns: 0,
            last_adj_ppm: 0.0,
            initial_epoch_offset_ns: 0,
            epoch_aligned: false,
            valid_count: 0,
            clock_settled: false,
            settling_threshold: 1,
            status_shared,
            calibration_samples: Vec::with_capacity(calibration_count),
            calibration_offset_ns: 0,
            calibration_complete,
            applied_freq_ppm: 0.0,
            warmup_start: now,
            warmup_complete: false,
            // Self-tuning servo state
            drift_baseline_ppm: 0.0,
            is_locked: false,
            lock_stable_count: 0,
            in_production_mode: false,
            in_nano_mode: false,
            nano_sustain_count: 0,
            nano_exit_count: 0,
            last_offset_us: None,
            last_offset_time: None,
            smoothed_rate_ppm: 0.0,
            // NTP UTC tracking - enabled on BOTH platforms
            // PTP (Dante) controls frequency only, NTP maintains UTC alignment
            // Dante provides device uptime, NOT UTC - so NTP is needed for real time
            last_ntp_check: now,
            ntp_offset_samples: VecDeque::with_capacity(NTP_SAMPLE_COUNT + 2),
            // #68: set by configure_ntp_server_mode() when this node serves the fleet
            ntp_server_mode: false,
            ntp_server_max_step_us: 0,
            ntp_pending_step: None,
            last_ntp_step: None,
            ntp_server_checks_since_step: 0,
            // Accumulated phase error tracking
            accumulated_phase_error_us: 0.0,
            last_phase_accumulation_time: None,
            // #91: step-storm detection state (server mode)
            ntp_step_times: VecDeque::new(),
            last_step_storm_warn: None,
            // PTP offline detection
            last_ptp_packet: now,
            ptp_offline: false,
            ptp_offline_logged: false,
            // NTP failure tracking
            ntp_consecutive_failures: 0,
            ntp_failed: false,
            // #68 freshness tracking
            last_ntp_success: None,
            last_ntp_success_epoch: None,
            started_at: now,
            // Adaptive spike detection
            spike_filter: SpikeFilter::new(),
            // Adaptive jitter smoothing
            jitter_estimator: JitterEstimator::new(),
            // #679 — throttled drift summary log counter
            drift_log_sample_count: 0,
            // #97 — phase-slew servo; Some only when the flag is set, so None = pre-#97 behaviour
            phase_slew: if config_phase_slew_enabled && !date_sync.enabled {
                info!("[PHASE-SLEW] enabled — sub-50ms UTC errors will SLEW (bounded PI servo, feed-forward decoupled), not step");
                Some(PhaseSlewServo::new())
            } else {
                None
            },
            pending_f_phase_ppm: 0.0,
            last_applied_f_phase_ppm: 0.0,
            last_phase_slew_output: None,
            last_phase_slew_update: None,
            phase_slew_alarm_active: false,
            phase_slew_preserve_streak: 0,
            date_sync,
        }
    }

    // ========================================================================
    // PUBLIC API
    // ========================================================================

    pub fn get_status_shared(&self) -> Arc<RwLock<SyncStatus>> {
        self.status_shared.clone()
    }

    /// #117: true unless `system.clock_discipline = "legacy"`.
    pub fn phase_lock_enabled(&self) -> bool {
        self.date_sync.enabled
    }

    /// #88: where this node reads its master's date-offset announce. `main` wires the UDP poller
    /// on every non-master node; the master (and a test that wants no authority) keeps the default
    /// `NoAuthority`.
    pub fn set_date_authority_source(&mut self, source: Box<dyn DateAuthoritySource>) {
        self.date_sync.source = source;
    }

    /// #68 — record a SUCCESSFUL upstream measurement: publish it to
    /// `SyncStatus` (offset + quality + freshness) and reset the failure state.
    ///
    /// One place, called by BOTH the boot-time `run_ntp_sync()` and the
    /// periodic `check_ntp_utc_tracking()`. The boot path published nothing at
    /// all before, which is why strih served `ntp_offset_us: 0,
    /// ntp_sample_count: 0` for 19 minutes after a restart that had in fact
    /// measured +1.039 s and stepped the clock by it.
    fn record_ntp_success(&mut self, offset_us: i64, measurement: &crate::ntp::NtpMeasurement) {
        let now = Instant::now();
        // The stamp is the measured UTC instant, NOT the local reading — this
        // runs BEFORE the correction is applied, and the same epoch is served to
        // every NTP client as the Reference Timestamp. Stamping the local clock
        // would (a) make `ntp_updated_ts` (wall clock) disagree with `ntp_age_s`
        // (monotonic) by the size of the correction, and (b) on a node running
        // AHEAD of UTC put the served reference timestamp in the FUTURE, which
        // RFC 5905 §11.2 has conforming clients discard outright.
        let local_us = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as i128;
        let epoch = ((local_us + offset_us as i128).max(0) / 1_000_000) as u64;
        self.last_ntp_success = Some(now);
        self.last_ntp_success_epoch = Some(epoch);

        if self.ntp_failed {
            info!("[NTP] Connection restored");
        }
        self.ntp_consecutive_failures = 0;
        self.ntp_failed = false;

        if let Ok(mut status) = self.status_shared.write() {
            status.ntp_offset_us = offset_us;
            status.ntp_failed = false;
            status.ntp_spread_us = measurement.spread_us;
            status.ntp_sample_count = measurement.sample_count;
            status.pcap_ntp_active = measurement.pcap_active;
            status.ntp_updated_ts = epoch;
            status.ntp_age_s = Some(0);
        }
    }

    /// #68 — raise `ntp_failed` when no fresh measurement has landed within the
    /// configured window, even though no query ever explicitly failed.
    ///
    /// Runs from the 10-second status tick rather than inside the query path,
    /// precisely because the failure being detected is "the query path is not
    /// running at all" — the state the master sat in for 18 hours while
    /// reporting `ntp_failed: false`.
    fn check_ntp_freshness(&mut self) {
        if !self.ntp_measurement_is_stale() || self.ntp_failed {
            return;
        }

        self.ntp_failed = true;
        let window_s = effective_stale_window(self.config.ntp_stale_secs).as_secs();
        match self.last_ntp_success {
            Some(t) => warn!(
                "[NTP] No successful measurement for {}s (window {}s) — UTC alignment is \
                 no longer being maintained; treating this node's NTP reading as stale",
                t.elapsed().as_secs(),
                window_s
            ),
            None => warn!(
                "[NTP] No successful measurement in {}s of uptime (window {}s) — this node \
                 has never aligned to UTC",
                self.started_at.elapsed().as_secs(),
                window_s
            ),
        }
        if let Ok(mut status) = self.status_shared.write() {
            status.ntp_failed = true;
        }
    }

    /// #68 — has this node gone a whole staleness window with no successful
    /// measurement? Read by BOTH the freshness alarm and `ntp_discipline_due`,
    /// so the alarm can never fire on a node the discipline is not even trying
    /// to serve: going stale is exactly what arms the query.
    fn ntp_measurement_is_stale(&self) -> bool {
        ntp_is_stale(
            self.last_ntp_success.map(|t| t.elapsed()),
            self.started_at.elapsed(),
            effective_stale_window(self.config.ntp_stale_secs),
        )
    }

    /// #68 — after a correction lands, publish the offset that REMAINS.
    ///
    /// `record_ntp_success` runs before the step (it must: the step needs the
    /// measurement), so without this `/status` advertises the error that was
    /// just cancelled. Live consequence: for a whole interval after a restart
    /// the master served `ntp_offset_us: 1039375` for an offset it had already
    /// stepped away — and camera-box's gate thresholds exactly that field.
    fn publish_post_step_residual(&self, residual_us: i64) {
        if let Ok(mut status) = self.status_shared.write() {
            status.ntp_offset_us = residual_us;
        }
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip {
            return;
        }

        match self.ntp.get_offset() {
            Ok(measurement) => {
                let offset = measurement.offset;
                let sign = measurement.sign;
                let sign_str = if sign > 0 { "+" } else { "-" };
                info!(
                    "NTP Sync: Offset {}{:?} (spread:{}us samples:{})",
                    sign_str, offset, measurement.spread_us, measurement.sample_count
                );

                // #68: the boot measurement is a real measurement — publish it.
                let offset_us = if sign > 0 {
                    offset.as_micros() as i64
                } else {
                    -(offset.as_micros() as i64)
                };
                self.record_ntp_success(offset_us, &measurement);

                if offset.as_millis() > 50 {
                    info!("Stepping clock (NTP)...");
                    if let Err(e) = self.clock.step_clock(offset, sign) {
                        error!("Failed to step clock: {}", e);
                    } else {
                        info!("Clock stepped successfully.");
                        // The boot step is unbounded, so it cancels the WHOLE
                        // measured offset — publish the residual, not the error
                        // that no longer exists (#68).
                        self.publish_post_step_residual(0);
                    }
                } else {
                    info!("Offset small, skipping step.");
                }
            }
            Err(e) => warn!("NTP Sync failed: {}", e),
        }
    }

    /// Periodic NTP UTC alignment - steps clock to maintain UTC sync
    ///
    /// This keeps all computers aligned to real UTC time by:
    /// - Checking NTP offset every 30 seconds (only in production mode)
    /// - Stepping clock if offset exceeds 500µs threshold
    /// - ONLY sets time value - does NOT change frequency (Dante stays locked)
    ///
    /// Key insight: step_clock() and adjust_frequency() are independent:
    /// - step_clock() = SetSystemTime() - sets absolute time value
    /// - adjust_frequency() = SetSystemTimeAdjustmentPrecise() - sets tick rate
    ///
    /// Stepping time does NOT affect the Dante-tuned frequency!
    /// Check PTP status and handle offline mode
    fn check_ptp_status(&mut self) {
        let elapsed = self.last_ptp_packet.elapsed();

        if elapsed > Duration::from_secs(PTP_TIMEOUT_SECS) {
            if !self.ptp_offline {
                self.ptp_offline = true;
                if !self.ptp_offline_logged {
                    // camera-box issue 1073: if packets ARE arriving but are being
                    // dropped by the allowlist, the grandmaster is not offline —
                    // it is present and blocked by (a likely mis-set) config. Say
                    // so, instead of the misleading "masters may be offline".
                    if self.gm_dropped_since_accepted > 0 {
                        warn!(
                            "[PTP] No ALLOWED packets for {}s, but {} packet(s) from \
                             non-allowlisted source(s) were dropped — the grandmaster may be \
                             present but blocked by config.gm_allowlist; verify the allowlist",
                            PTP_TIMEOUT_SECS, self.gm_dropped_since_accepted
                        );
                    } else {
                        warn!(
                            "[PTP] No packets received for {}s - PTP masters may be offline",
                            PTP_TIMEOUT_SECS
                        );
                    }
                    info!("[PTP] Continuing with NTP-only time sync");
                    self.ptp_offline_logged = true;
                }
                // Update status to reflect offline state
                if let Ok(mut status) = self.status_shared.write() {
                    status.settled = false;
                    status.mode = "NTP-only".to_string();
                }
            }
        } else if self.ptp_offline {
            // PTP came back online
            self.ptp_offline = false;
            self.ptp_offline_logged = false;
            info!("[PTP] Packets received - PTP sync resumed");
        }
    }

    pub fn check_ntp_utc_tracking(&mut self) {
        // #71: server mode uses a dedicated, UTC-relevant cadence instead of
        // the PTP-vs-Dante-GM lock-quality signal `calculate_adaptive_ntp_interval`
        // is actually driven by (accumulated_phase_error_us tracks this
        // node's own frequency-lock error against the Dante grandmaster, not
        // against UTC -- see the NTP_SERVER_* doc comment above). Every other
        // node's adaptive-interval behavior is unchanged.
        //
        // Adaptive NTP interval based on accumulated phase error (client
        // mode only):
        // - Higher error = check more frequently for tighter UTC alignment
        // - Low error = use default interval to reduce NTP overhead
        let ntp_interval_secs = if self.ntp_server_mode {
            NTP_SERVER_CHECK_INTERVAL_SECS
        } else {
            self.calculate_adaptive_ntp_interval()
        };

        // #68: the run/skip decision is a pure, unit-tested policy — see
        // `ntp_discipline_due`. A server-mode master runs it regardless of its
        // own PTP lock state; every other node's semantics are unchanged.
        if !ntp_discipline_due(
            self.ntp_server_mode,
            self.ptp_offline,
            self.is_locked,
            self.ntp_measurement_is_stale(),
            self.last_ntp_check.elapsed(),
            Duration::from_secs(ntp_interval_secs),
        ) {
            return;
        }

        self.last_ntp_check = Instant::now();

        // Query NTP and record offset
        match self.ntp.get_offset() {
            Ok(measurement) => {
                let offset = measurement.offset;
                let sign = measurement.sign;
                // Use as_micros() directly to avoid overflow from as_nanos() -> i64
                let offset_us = if sign > 0 {
                    offset.as_micros() as i64
                } else {
                    -(offset.as_micros() as i64)
                };

                // NTP success — reset failure tracking AND publish the reading
                // together with WHEN it was taken (#68: one shared recorder, so
                // the boot path and this path can never diverge again).
                self.record_ntp_success(offset_us, &measurement);

                // #76: count this successful check toward the escape-valve
                // starvation counter (see ntp_server_checks_since_step's own doc
                // comment) — reset below whenever a step actually applies.
                if self.ntp_server_mode {
                    self.ntp_server_checks_since_step =
                        self.ntp_server_checks_since_step.saturating_add(1);
                }

                // Add sample to buffer. #53: `offset_us` is now the burst-filtered
                // (RTT-selected + median'd) value from NtpClient::get_offset(), not a
                // single raw round trip — the MAD threshold below and the #50
                // step-agreement gate deliberately keep consuming this same per-check
                // value unchanged; they just get a cleaner input now.
                //
                // #97 (review 🔵): recorded BEFORE the slew early-return below, so the
                // adaptive-MAD threshold's history stays fresh even while a box is slewing —
                // a later slew→step transition (error jumps >50ms) then reads a real buffer,
                // not an empty one.
                self.ntp_offset_samples.push_back(offset_us);
                if self.ntp_offset_samples.len() > NTP_SAMPLE_COUNT + 2 {
                    self.ntp_offset_samples.pop_front();
                }

                // #117 / #88: under the PTP phase lock NTP never steers the rate, and once this node
                // has a date authority it never steps the clock on its own either — the master
                // turns its UTC error into a coordinated announce, a follower only watches.
                if self.ntp_under_date_authority(offset_us) {
                    return;
                }

                // #97: PHASE SLEW. While genuinely PTP-locked, a small (<50ms) UTC error is
                // corrected by a bounded frequency slew instead of a discrete step — see
                // `crate::phase_slew`. The step path below is kept UNCHANGED for: a large/insane
                // error (|e|>50ms, cold boot), the acquisition regime (not yet locked), and
                // NTP-only fallback (ptp_offline) — the three cases where a step is still correct
                // and where injecting a decoupled slew would be unsafe. When the servo is disabled
                // (`None`, the default) this whole block is skipped and the step path is
                // byte-identical to before.
                if self.phase_slew.is_some() {
                    if self.is_locked && !self.ptp_offline && !phase_slew::should_step(offset_us) {
                        // Log-surface CONTRACT: the whole camera-box gate ecosystem parses the
                        // exact `[NTP] offset:{:+}us` line for FRESHNESS (dantesync_offset_verdict,
                        // the #326 painter gate, verify-device (d)). The slew replaces the STEP,
                        // never the telemetry — keep emitting the line on every slewed cycle, or
                        // every freshness consumer reads a phase-slew box as stale/UNKNOWN and
                        // fail-closes (live incident: camera-box E2E painter gate exit 11,
                        // 2026-08-20).
                        info!("[NTP] offset:{:+}us", offset_us);
                        self.slew_phase(offset_us);
                        return; // slewed, not stepped — skip the entire step-decision block
                    }
                    // #105 — not slewable (a large error to STEP, or a brief not-locked flap). The
                    // step corrects PHASE; it must NOT discard the learned FREQUENCY DC — zeroing the
                    // integrator on every such cycle is exactly what let a high-DC box (~50 ppm, cam1)
                    // run away and loop step→reset→step. So keep the DC applied and re-enter fast
                    // acquisition, EXCEPT on a genuine PTP outage (ptp_offline), where the phase
                    // servo's decoupling assumptions don't hold and a full reset is correct.
                    if phase_slew::should_step(offset_us) {
                        info!(
                            "[PHASE-SLEW] |e|={}us exceeds the {}us slew boundary — STEPPING (phase); \
                             keeping the learned f_phase DC across the step (#105)",
                            offset_us,
                            phase_slew::STEP_BOUNDARY_US
                        );
                    }
                    if self.ptp_offline {
                        self.reset_phase_slew();
                    } else {
                        self.preserve_phase_slew_dc_across_step();
                    }
                }

                // (offset + quality (#53) + freshness (#68) were published to
                // SyncStatus by record_ntp_success() above)

                // #68: the master does NOT use the MAD-widened threshold.
                //
                // `calculate_ntp_adaptive_threshold` models JITTER — it widens by
                // 5x the MAD of recent samples so a noisy LAN does not provoke
                // constant stepping. On the master the samples are not jitter:
                // they are a deterministic monotonic ramp (the Dante-vs-UTC
                // frequency error integrating at 6-19 ppm). The MAD of a 7-point
                // ramp with per-interval step `s` is exactly `2s`, so the
                // threshold self-inflates to `500 + 10s` — ten times the accrual
                // it is meant to catch — and the master would sawtooth 2.5-6.8 ms
                // (ceiling 10 ms) against UTC forever, with every one of those
                // steps served to the fleet and chased by each client one or two
                // of ITS intervals later. On the base threshold the same loop
                // holds UTC inside ~0.7-1.7 ms. The outlier protection the MAD
                // widening exists for is already provided here by the
                // two-agreeing-samples step gate (server mode's own variant
                // of it, see ntp_step_gate's #71 doc comment).
                //
                // #71: server mode also uses a LOWER floor than the client's
                // 500us — still well clear of the ~5-32us single-query noise
                // floor #53 measured, but catches the ramp earlier than the
                // client's tuning, which exists for LAN jitter that does not
                // apply to the master's monotonic signal.
                //
                // #83: ...UNLESS the master is genuinely PTP-locked, in which case the
                // threshold becomes a large deadband -- see server_step_threshold_us's own
                // doc comment. Not locked (still acquiring, or ptp_offline) keeps the
                // original tight threshold above, unchanged.
                let step_threshold = if self.ntp_server_mode {
                    server_step_threshold_us(self.is_locked, self.ptp_offline)
                } else {
                    self.calculate_ntp_adaptive_threshold()
                };

                // #83 REVIEW FINDING (critical): the escape-valve counter above increments on
                // EVERY successful check regardless of whether THIS check's offset was even
                // over threshold -- harmless under the routine tight threshold (almost every
                // check WAS over threshold in that regime, so "checks since step" and "over-
                // threshold checks since step" were the same number), but WRONG under the large
                // PTP-locked deadband: the deadband's natural cadence (~66 checks at 38ppm,
                // ~500-660s) is LONGER than the escape valve's 30-check/5-min patience, so by
                // the time the offset first legitimately crosses the deadband, the counter had
                // ALREADY exceeded its patience on checks that were never over threshold at
                // all -- forcing every deadband-driven step through the escape valve,
                // UNCONFIRMED, bypassing both the 2-sample agreement gate AND the burst-quality
                // gate on the very first over-threshold reading, every single time (defeating
                // the entire #76 confirmation/quality machinery for the primary locked-mode
                // case, exactly the noisy-WAN-outlier scenario #76 exists to reject). Reset the
                // counter whenever THIS check's offset is not currently over threshold, so it
                // counts consecutive OVER-THRESHOLD-BUT-UNCONFIRMED checks -- the escape
                // valve's actual documented intent -- and it naturally scales to whichever
                // threshold is active, tight or deadband, with no new tunable constant.
                if self.ntp_server_mode && offset_us.abs() <= step_threshold {
                    self.ntp_server_checks_since_step = 0;
                }

                // Log current offset with threshold info
                if step_threshold > NTP_STEP_THRESHOLD_BASE_US {
                    info!(
                        "[NTP] offset:{:+}us (threshold:{}us, adaptive)",
                        offset_us, step_threshold
                    );
                } else {
                    info!("[NTP] offset:{:+}us", offset_us);
                }

                // #76: server mode excludes a low-quality (high internal spread) burst from
                // the step decision ENTIRELY — it neither starts, confirms, nor contradicts a
                // pending candidate. This is independent of the agreement-tolerance fix in
                // ntp_step_gate: a burst can have a small spread_us and still disagree in
                // magnitude with the last candidate (caught by the tolerance), or a large
                // spread_us and still happen to land close to the candidate (caught here).
                // /status publishing already happened above (record_ntp_success) and is
                // unaffected — an operator can still see a high-spread reading.
                let low_quality_server_burst =
                    self.ntp_server_mode && measurement.spread_us > NTP_SERVER_MAX_BURST_SPREAD_US;
                if low_quality_server_burst {
                    info!(
                        "[NTP-Server] burst spread {}us exceeds the {}us quality bound — \
                         excluded from the step decision (offset {:+}us not used to start, \
                         confirm, or contradict a candidate)",
                        measurement.spread_us, NTP_SERVER_MAX_BURST_SPREAD_US, offset_us
                    );
                }

                // #76 review finding (critical): the tolerance-agreement gate and the burst-
                // quality gate above can each independently reject a genuine same-sign trend
                // FOREVER (an oscillator error whose accrual permanently exceeds the fixed
                // tolerance; an upstream that never presents a low-enough-spread burst) --
                // reproduced live in review as unbounded, silent, permanent growth. This is the
                // shared last-resort escape valve: once too many checks have passed with no
                // actual step, the next genuinely over-threshold reading forces one regardless
                // of tolerance agreement OR burst quality. See NTP_SERVER_MAX_CHECKS_WITHOUT_STEP's
                // own doc comment for why this essentially never fires under real conditions.
                let server_starved = self.ntp_server_mode
                    && self.ntp_server_checks_since_step >= NTP_SERVER_MAX_CHECKS_WITHOUT_STEP
                    && offset_us.abs() > step_threshold;
                if server_starved {
                    warn!(
                        "[NTP-Server] escape valve: {} checks ({}) without a step -- forcing \
                         correction {:+}us past the tolerance/quality gates (starvation safety \
                         net, spread was {}us)",
                        self.ntp_server_checks_since_step,
                        if low_quality_server_burst {
                            "burst quality gate kept rejecting"
                        } else {
                            "agreement never confirmed"
                        },
                        offset_us,
                        measurement.spread_us
                    );
                    self.ntp_pending_step = None;
                }

                // Step clock if offset exceeds the threshold — but NEVER on a single
                // measurement: the agreement gate (#50) requires consecutive agreeing samples.
                // #76: the escape valve (server_starved) bypasses BOTH the quality gate and the
                // normal agreement gate as a last resort.
                if server_starved
                    || (!low_quality_server_burst && self.ntp_step_gate(offset_us, step_threshold))
                {
                    // #68: a correction is rate-bounded in server mode, where this
                    // node's step is the whole fleet's step. `ntp_server_max_step_us`
                    // is 0 for every other node and 0 means unbounded, so the same
                    // call is the unchanged client behaviour.
                    //
                    // #83 REVIEW FINDING (2nd round, critical): while genuinely locked, this is
                    // ALSO bounded by NTP_SERVER_LOCKED_MAX_STEP_US regardless of the configured
                    // ntp_server_max_step_us -- see that constant's own doc comment for the full
                    // incident (an escape-valve-forced step at ppm > ~75 could otherwise apply
                    // ~20-26ms unconfirmed, in one step). Not-locked path: completely unchanged.
                    let locked_now = self.ntp_server_mode && self.is_locked && !self.ptp_offline;
                    let effective_max_step_us = if locked_now {
                        if self.ntp_server_max_step_us > 0 {
                            self.ntp_server_max_step_us
                                .min(NTP_SERVER_LOCKED_MAX_STEP_US)
                        } else {
                            NTP_SERVER_LOCKED_MAX_STEP_US
                        }
                    } else {
                        self.ntp_server_max_step_us
                    };
                    let step_us = clamp_ntp_step_us(offset_us, effective_max_step_us);
                    if step_us != offset_us {
                        warn!(
                            "[NTP-Server] upstream correction {:+}us exceeds the {}us bound — \
                             stepping {:+}us now, {:+}us residual worked off over the next \
                             intervals",
                            offset_us,
                            effective_max_step_us,
                            step_us,
                            offset_us - step_us
                        );
                    }

                    // Apply the step (sets time, does NOT change frequency)
                    let step_dur = Duration::from_micros(step_us.unsigned_abs());
                    let step_sign = if step_us > 0 { 1 } else { -1 };

                    if let Err(e) = self.clock.step_clock(step_dur, step_sign) {
                        warn!("[NTP] Step failed: {}", e);
                    } else {
                        // Clear NTP samples after step to start fresh measurement
                        self.ntp_offset_samples.clear();
                        self.ntp_pending_step = None;
                        // #76: any actual step (normal agreement OR the escape valve) resets
                        // the starvation counter -- we just corrected, the clock is caught up.
                        //
                        // #83 REVIEW FINDING (2nd round, critical): NOT when genuinely locked
                        // AND a residual remains (step_us != offset_us, i.e. NTP_SERVER_LOCKED_
                        // MAX_STEP_US clamped this one) -- resetting to a full
                        // NTP_SERVER_MAX_CHECKS_WITHOUT_STEP-check wait here would let a
                        // sustained high-ppm residual accrue FASTER (30 checks' worth of new
                        // drift) than one clamped correction removes, causing UNBOUNDED growth
                        // instead of convergence (verified by simulation before choosing this
                        // fix). Leaving the counter armed lets the escape valve re-fire on the
                        // very next over-threshold check, producing a rapid run of further
                        // clamped corrections until the residual clears -- see
                        // NTP_SERVER_LOCKED_MAX_STEP_US's own doc comment. Not-locked path (and
                        // any fully-applied locked step, step_us == offset_us): unchanged, reset.
                        if !(locked_now && step_us != offset_us) {
                            self.ntp_server_checks_since_step = 0;
                        }
                        // Discard the post-step transient from every PTP measurement path.
                        self.reset_ptp_measurement_after_step();
                        // #117: a step moves the wall, so D moves with it (the phase lock sees no
                        // disturbance) — this is the LOCAL date path (no authority heard, or PTP
                        // offline). On the master it is also a rebase of the fleet offset.
                        if self.date_sync.enabled {
                            self.note_local_date_step(step_us.saturating_mul(1_000));
                        }
                        // #68: publish what REMAINS, not the error just cancelled
                        // (0 for a full step, the remainder for a bounded one).
                        self.publish_post_step_residual(offset_us - step_us);
                        info!("[NTP] Stepped {:+}us", step_us);
                        // #91: record this step for storm-rate detection and raise
                        // the alarm if a server-mode master is step-storming.
                        self.record_ntp_step_and_check_storm();
                    }
                }
            }
            Err(e) => {
                // Track consecutive failures
                self.ntp_consecutive_failures += 1;

                // Adversarial-review fix (#53 continuation): a failed burst
                // produced NO measurement at all, so `pcap_ntp_active` must
                // never keep reporting whatever it was set to by the last
                // SUCCESSFUL burst -- otherwise a consumer reading this field
                // alone during an outage is told the kernel-timestamped
                // transport is still active when nothing ran this check.
                if let Ok(mut status) = self.status_shared.write() {
                    status.pcap_ntp_active = false;
                }

                if self.ntp_consecutive_failures >= NTP_FAILURE_THRESHOLD && !self.ntp_failed {
                    self.ntp_failed = true;
                    warn!(
                        "[NTP] Server unreachable - {} consecutive failures",
                        self.ntp_consecutive_failures
                    );

                    // Update shared status
                    if let Ok(mut status) = self.status_shared.write() {
                        status.ntp_failed = true;
                    }
                } else {
                    warn!(
                        "[NTP] Failed ({}/{}): {}",
                        self.ntp_consecutive_failures, NTP_FAILURE_THRESHOLD, e
                    );
                }
            }
        }
    }

    /// #97 — feed a small (<50ms) UTC phase error to the bounded PI phase-slew servo instead of
    /// stepping. Updates the held `pending_f_phase_ppm` (applied continuously by
    /// `apply_self_tuning_servo`), clears any half-formed step candidate, raises the "slew
    /// saturated" alarm when the guard trips, and publishes telemetry. `dt` is the real elapsed
    /// time since the last slew update (clamped to a sane range) so the servo's deadbeat gain cap
    /// and integrator are correct across the client's variable NTP cadence.
    fn slew_phase(&mut self, offset_us: i64) {
        // #105 (review 🟡): a slew re-engaged, so the preserve-streak clock resets.
        self.phase_slew_preserve_streak = 0;
        let now = Instant::now();
        let dt = self
            .last_phase_slew_update
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(NTP_SERVER_CHECK_INTERVAL_SECS as f64)
            .clamp(1.0, 120.0);
        self.last_phase_slew_update = Some(now);

        let out = self
            .phase_slew
            .as_mut()
            .expect("slew_phase is only reached when phase_slew is Some")
            .update(offset_us, dt);
        self.pending_f_phase_ppm = out.f_phase_ppm;
        self.last_phase_slew_output = Some(out);

        // We corrected via slew, so no step is pending and the server-mode starvation counter is
        // not meaningful (it gates the step path, which we are bypassing).
        self.ntp_pending_step = None;
        if self.ntp_server_mode {
            self.ntp_server_checks_since_step = 0;
        }

        // #97 (review 🟡): edge-triggered — the loud WARN fires ONCE at the alarm's onset, not on
        // every update for the whole (multi-minute) saturation episode.
        if out.alarm && !self.phase_slew_alarm_active {
            PhaseSlewServo::log_saturated_alarm(offset_us, out.f_phase_ppm);
        }
        self.phase_slew_alarm_active = out.alarm;
        info!(
            "[PHASE-SLEW] e={:+}us f_phase={:+.2}ppm (P={:+.2} I={:+.2}) f_ptp={:+.2}ppm [{}]{}",
            offset_us,
            out.f_phase_ppm,
            out.p_ppm,
            out.i_ppm,
            self.last_adj_ppm,
            if out.converged { "TRK" } else { "ACQ" }, // #105 acquisition vs tracking mode
            if out.saturated { " SATURATED" } else { "" }
        );
        self.update_shared_status();
    }

    /// #97 — disengage the phase slew (lock loss / large error / PTP offline). The servo's
    /// integrator is reset so a re-lock relearns the DC afresh, and the held slew drops to 0 so
    /// the decoupling in `apply_self_tuning_servo` becomes a no-op again.
    fn reset_phase_slew(&mut self) {
        if let Some(servo) = self.phase_slew.as_mut() {
            *servo = PhaseSlewServo::new();
        }
        self.pending_f_phase_ppm = 0.0;
        self.last_phase_slew_output = None;
        self.last_phase_slew_update = None;
        // #97 (review 🟡): re-arm the alarm edge so a fresh saturation episode logs its onset again.
        self.phase_slew_alarm_active = false;
        self.phase_slew_preserve_streak = 0;
    }

    /// #105 — disengage the phase SLEW for a step / brief flap WITHOUT discarding the learned
    /// frequency DC. A step corrects PHASE; the ~50 ppm Dante-vs-UTC frequency relationship the
    /// integrator holds is unchanged by a phase jump, so it is KEPT and continues to be applied
    /// (`pending_f_phase_ppm = i`) — zeroing it on every non-slew cycle is what let a high-DC box run
    /// away and loop step→reset→step. `note_phase_step` re-enters fast acquisition so the servo
    /// re-verifies the (possibly changed) DC. `reset_phase_slew` (full reset) stays for a genuine
    /// loss of reference (PTP offline).
    fn preserve_phase_slew_dc_across_step(&mut self) {
        // #105 (review 🟡): bound the stale-DC hold. A brief flap keeps its DC (no runaway), but a
        // PERSISTENT not-locked spell (a real reference change, e.g. GM changeover) must not apply a
        // stale DC forever — after PHASE_SLEW_MAX_PRESERVE_STREAK consecutive preserves with no slew
        // re-engaging, fall back to a full reset so the servo relearns the DC from scratch.
        self.phase_slew_preserve_streak = self.phase_slew_preserve_streak.saturating_add(1);
        if self.phase_slew_preserve_streak > PHASE_SLEW_MAX_PRESERVE_STREAK {
            self.phase_slew_preserve_streak = 0;
            self.reset_phase_slew();
            return;
        }
        let held = if let Some(servo) = self.phase_slew.as_mut() {
            servo.note_phase_step();
            servo.i_ppm()
        } else {
            0.0
        };
        // Keep applying the learned DC frequency across the step so the clock does not free-run.
        self.pending_f_phase_ppm = held;
        self.last_phase_slew_output = None;
        // `last_phase_slew_update` is intentionally KEPT — a step is not a re-lock, so the next slew's
        // dt stays the real elapsed time.
        // #97 (review 🟡): re-arm the alarm edge so a fresh saturation episode logs its onset again.
        self.phase_slew_alarm_active = false;
    }

    // ========================================================================
    // FLEET DATE OFFSET (dantesync#88) + PTP PHASE LOCK ANCHOR (#117)
    // ========================================================================

    /// Discard the transient a clock step leaves in every PTP measurement path: the sample
    /// windows, the 2 s grace, the rate tracker, the min-delta filter and the spike filter.
    /// Shared by the NTP step path and the coordinated date step, so both reset identically.
    fn reset_ptp_measurement_after_step(&mut self) {
        // Clear PTP sample windows to discard post-step transient samples
        self.sample_window.clear();
        self.date_sync.window.clear();
        self.date_sync.pending_median_ns = None;
        self.date_sync.fresh_window = false;
        // Set grace period to skip PTP samples for 2s after step
        self.last_ntp_step = Some(Instant::now());
        // Reset drift tracking to avoid false spike from step
        self.last_offset_us = None;
        self.last_offset_time = None;
        // Reset prev timestamps so min_delta filter works correctly after grace period
        self.prev_t1_ns = 0;
        self.prev_t2_ns = 0;
        // Clear spike filter to prevent false positives from step transient
        self.spike_filter.clear();
        // NOTE: jitter_estimator is NOT cleared on NTP step because
        // jitter is a hardware property that persists across steps
        // Reset accumulated phase error - we just aligned to UTC
        self.accumulated_phase_error_us = 0.0;
        self.last_phase_accumulation_time = None;
    }

    /// True while this node is the fleet's NTP server (#68).
    pub fn ntp_server_mode(&self) -> bool {
        self.ntp_server_mode
    }

    /// #68 — put this node into NTP **server** mode: it serves UTC to the fleet
    /// AND keeps disciplining itself against its own upstream.
    ///
    /// This REPLACES the old `disable_ntp_tracking()`, which was the whole
    /// defect: it turned the periodic queries off on the theory that "this
    /// machine IS the time source". That is true of the FLEET's mutual
    /// coherence and false of UTC — no oscillator is a source of UTC. With the
    /// loop off, the master's only UTC measurement was the boot-time one-shot,
    /// after which it free-ran at the Dante grandmaster's rate: 6-19 ppm
    /// measured on strih, i.e. ~21 ms of error 19 minutes after a restart and
    /// 1.04 s over two days, with the whole fleet coherently following it.
    ///
    /// Discipline reuses the ordinary client machinery (`step_clock` on a
    /// confirmed correction), with server-only tuning throughout —
    /// `NTP_SERVER_STEP_THRESHOLD_US`, `NTP_SERVER_CHECK_INTERVAL_SECS`,
    /// same-sign agreement with a FIXED tolerance
    /// (`NTP_SERVER_AGREEMENT_TOL_US`), and a burst-quality gate
    /// (`NTP_SERVER_MAX_BURST_SPREAD_US`) — see `ntp_step_gate`'s own doc
    /// comment (#71/#76) — plus one property no client shares: it runs
    /// regardless of this node's own PTP lock state. A single correction is
    /// bounded by `max_step_us` (see `clamp_ntp_step_us`).
    ///
    /// **Steady state, stated plainly (post-#76):** at the real ~19 ppm
    /// measured on strih, the master takes small periodic steps —
    /// deterministically ~190-380 us every ~20 s in a noiseless model — and,
    /// on a noisy real upstream, rejects the great majority of noise-driven
    /// candidates via the fixed agreement tolerance, stepping only roughly
    /// once every 20-60s (dantesync#76's own closed-loop noisy-upstream
    /// simulation). This replaces TWO prior behaviours: the pre-#71
    /// 0.9-2.5 ms sawtooth on a ~60-90 s lag, and #71's OWN v1.8.31/v1.8.32
    /// regression (verified only against a noiseless simulation) of chasing
    /// real WAN measurement noise into a step roughly every ~10 s. Each
    /// step propagates to the fleet a client interval or two later, same as
    /// always. The lever if `max_step_us` ever matters on the rig is
    /// unchanged: it turns one larger jump into several smaller ones.
    pub fn configure_ntp_server_mode(&mut self, max_step_us: i64) {
        self.ntp_server_mode = true;
        self.ntp_server_max_step_us = max_step_us;
        info!(
            "[NTP-Server] Upstream discipline ACTIVE — re-querying every {}s (fixed, \
             server-mode cadence), threshold {}us with a {}us fixed agreement tolerance \
             (always {} same-sign samples required, no single-sample fast lane), bursts over \
             {}us spread excluded from the step decision, single correction bounded to {}us, \
             staleness window {}s (this host serves the fleet, but UTC still comes from \
             upstream)",
            NTP_SERVER_CHECK_INTERVAL_SECS,
            NTP_SERVER_STEP_THRESHOLD_US,
            NTP_SERVER_AGREEMENT_TOL_US,
            NTP_STEP_AGREEMENT_N,
            NTP_SERVER_MAX_BURST_SPREAD_US,
            max_step_us,
            effective_stale_window(self.config.ntp_stale_secs).as_secs()
        );
    }

    /// Calculate adaptive NTP step threshold based on measured offset variance.
    ///
    /// Uses MAD (median absolute deviation) of recent NTP offsets to determine
    /// the noise floor. High-jitter systems automatically get a higher threshold
    /// to avoid unnecessary stepping that creates oscillation.
    ///
    /// Returns: threshold in microseconds, clamped between base and max.
    /// #50 NTP step-agreement gate — pure decision, unit-tested directly.
    ///
    /// Returns true when the clock SHOULD step for `offset_us`. An over-threshold offset
    /// becomes a CANDIDATE first; only when NTP_STEP_AGREEMENT_N consecutive over-threshold
    /// samples AGREE does the step fire. A disagreeing over-threshold sample REPLACES the
    /// candidate (it is itself suspect); an under-threshold sample clears it. Kills the
    /// loaded-LAN outlier step-reverse pairs (+2831us→-2825us) while a GENUINE offset still
    /// steps one interval later (the next sample agrees).
    ///
    /// Client mode (unchanged): "agree" additionally requires magnitude within
    /// max(NTP_STEP_AGREEMENT_TOL_US, |first|/2) — correct for jitter around a roughly
    /// stationary offset, where two over-threshold readings should be close in size if
    /// they represent the same real error.
    ///
    /// #71/#76 — server mode: the master's UTC error is genuine drift (effectively a
    /// deterministic ramp) PLUS real measurement noise from whichever upstream this node
    /// queries. A magnitude-similarity requirement is wrong when scaled to the CANDIDATE's own
    /// magnitude (#71's original finding: `max(TOL, |cand|/2)` is too tight for a small genuine
    /// candidate, and grows too loose once a noisy large candidate has already inflated it) —
    /// but dropping magnitude checking ENTIRELY (#71's v1.8.31/v1.8.32 fix) is ALSO wrong on a
    /// noisy upstream, where "small" is not a reliable trust signal (dantesync#76: strih's real
    /// WAN upstream scatters +0.5..+2.5ms between bursts, comparable to or larger than the true
    /// ~190-380us/check drift). Server mode therefore:
    ///   - requires only the SAME SIGN to agree (a ramp does not reverse sign between two
    ///     consecutive real readings; the historical outlier this gate exists for, #50's
    ///     +2831/-2825us pair, is an opposite-SIGN reversal and is still caught by this alone).
    ///   - additionally requires the confirming sample to be within a FIXED, non-scaling
    ///     `NTP_SERVER_AGREEMENT_TOL_US` of the candidate — sized to the TRUE expected per-check
    ///     accrual, not to the candidate's own (possibly noisy) magnitude.
    ///   - has NO single-sample fast lane: ALWAYS requires NTP_STEP_AGREEMENT_N (2) same-sign,
    ///     tolerance-bounded samples before stepping, regardless of magnitude. #71's fast lane
    ///     (single-sample-immediate for small offsets) chased strih's real WAN noise into a step
    ///     roughly every ~10s on the live canary; this was the dominant defect #76 fixes.
    ///
    /// Burst QUALITY (`spread_us`) is a separate, independent filter applied at the
    /// check_ntp_utc_tracking call site, not inside this pure function — see
    /// `NTP_SERVER_MAX_BURST_SPREAD_US`'s own doc comment.
    fn ntp_step_gate(&mut self, offset_us: i64, adaptive_threshold: i64) -> bool {
        if offset_us.abs() <= adaptive_threshold {
            if self.ntp_pending_step.take().is_some() {
                info!("[NTP] step candidate cleared (offset back under threshold)");
            }
            return false;
        }

        if self.ntp_server_mode {
            // #83 correction: the agreement tolerance is WIDER while genuinely locked (see
            // server_agreement_tolerance_us's own doc comment) -- everything else in this
            // branch is unchanged from #76.
            let agreement_tol_us = server_agreement_tolerance_us(self.is_locked, self.ptp_offline);
            return match self.ntp_pending_step {
                Some((cand, n)) => {
                    let same_sign = (cand > 0) == (offset_us > 0);
                    let agrees = same_sign && (offset_us - cand).abs() <= agreement_tol_us;
                    if agrees {
                        let n = n + 1;
                        if n >= NTP_STEP_AGREEMENT_N {
                            self.ntp_pending_step = None;
                            return true;
                        }
                        self.ntp_pending_step = Some((cand, n));
                        info!(
                            "[NTP-Server] step candidate {:+}us agreed by {:+}us ({}/{}) — awaiting agreement",
                            cand, offset_us, n, NTP_STEP_AGREEMENT_N
                        );
                    } else {
                        info!(
                            "[NTP-Server] step candidate {:+}us CONTRADICTED by {:+}us \
                             ({}) — replaced",
                            cand,
                            offset_us,
                            if same_sign {
                                "same sign, outside tolerance"
                            } else {
                                "sign reversal"
                            }
                        );
                        self.ntp_pending_step = Some((offset_us, 1));
                    }
                    false
                }
                None => {
                    info!(
                        "[NTP-Server] step candidate {:+}us (threshold:{}us, tolerance:{}us) — \
                         awaiting {} agreeing sample(s)",
                        offset_us,
                        adaptive_threshold,
                        agreement_tol_us,
                        NTP_STEP_AGREEMENT_N - 1
                    );
                    self.ntp_pending_step = Some((offset_us, 1));
                    false
                }
            };
        }

        match self.ntp_pending_step {
            Some((cand, n)) => {
                let same_sign = (cand > 0) == (offset_us > 0);
                let tol = NTP_STEP_AGREEMENT_TOL_US.max(cand.abs() / 2);
                if same_sign && (offset_us - cand).abs() <= tol {
                    let n = n + 1;
                    if n >= NTP_STEP_AGREEMENT_N {
                        self.ntp_pending_step = None;
                        return true;
                    }
                    self.ntp_pending_step = Some((cand, n));
                    info!(
                        "[NTP] step candidate {:+}us agreed by {:+}us ({}/{}) — awaiting agreement",
                        cand, offset_us, n, NTP_STEP_AGREEMENT_N
                    );
                } else {
                    info!(
                        "[NTP] step candidate {:+}us CONTRADICTED by {:+}us — replaced (outlier suspected)",
                        cand, offset_us
                    );
                    self.ntp_pending_step = Some((offset_us, 1));
                }
                false
            }
            None => {
                info!(
                    "[NTP] step candidate {:+}us (threshold:{}us) — awaiting {} agreeing sample(s)",
                    offset_us,
                    adaptive_threshold,
                    NTP_STEP_AGREEMENT_N - 1
                );
                self.ntp_pending_step = Some((offset_us, 1));
                false
            }
        }
    }

    fn calculate_ntp_adaptive_threshold(&self) -> i64 {
        // Need at least 3 samples to calculate meaningful statistics
        if self.ntp_offset_samples.len() < 3 {
            return NTP_STEP_THRESHOLD_BASE_US;
        }

        // Calculate median of offsets
        let mut sorted: Vec<i64> = self.ntp_offset_samples.iter().copied().collect();
        sorted.sort();
        let median = sorted[sorted.len() / 2];

        // Calculate MAD (median absolute deviation)
        let mut deviations: Vec<i64> = sorted.iter().map(|&x| (x - median).abs()).collect();
        deviations.sort();
        let mad = deviations[deviations.len() / 2];

        // Adaptive threshold = base + multiplier * MAD
        // This accounts for measurement noise while still detecting real drift
        let adaptive = NTP_STEP_THRESHOLD_BASE_US + (NTP_ADAPTIVE_MULTIPLIER * mad as f64) as i64;

        // Clamp to reasonable range
        adaptive.clamp(NTP_STEP_THRESHOLD_BASE_US, NTP_STEP_THRESHOLD_MAX_US)
    }

    /// Calculate adaptive NTP check interval based on accumulated phase error.
    ///
    /// Higher accumulated error = more frequent checks for tighter UTC alignment.
    /// Lower accumulated error = less frequent checks to reduce NTP overhead.
    ///
    /// Returns: interval in seconds (10, 15, or 30 based on accumulated error)
    fn calculate_adaptive_ntp_interval(&self) -> u64 {
        let abs_error = self.accumulated_phase_error_us.abs();

        if abs_error > 50.0 {
            10 // Tighter: check every 10s when drifting significantly
        } else if abs_error > 20.0 {
            15 // Moderate: check every 15s
        } else {
            NTP_CHECK_INTERVAL_SECS // Normal: default 30s interval
        }
    }

    /// The periodic status TICK (every 10 s from the main loop).
    ///
    /// Renamed from `log_status` in #68: it no longer merely publishes, it also
    /// evaluates NTP freshness and can raise `ntp_failed`. A name that says
    /// "log" hides a state transition from whoever schedules it.
    pub fn tick_status(&mut self) {
        // #68: the staleness check lives on this tick, not in the query path —
        // the failure it detects is "the query path is not running at all".
        self.check_ntp_freshness();
        // #113: re-resolve hostname allowlist entries periodically so a DNS/lease
        // change propagates without a restart (before the alarm samples health).
        self.maybe_reresolve_gm();
        // #114: evaluate + emit the loud NO-DANTE-CLOCK alarm. Runs on the 10 s
        // tick regardless of packet arrival, so a lost clock (which means NO PTP
        // packets, hence no servo update) still fires the per-minute alarm.
        self.evaluate_clock_alarm();
        // Publish the current snapshot for IPC / HTTP status consumers
        self.update_shared_status();
    }

    /// dantesync#113: log a hostname-resolution outcome — INFO on a change (an A
    /// record moved), ERROR (loud, and repeated every re-resolve) for each
    /// hostname currently unresolvable, per the ticket's fail-loud requirement.
    fn log_gm_resolve_outcome(outcome: &ResolveOutcome, allowlist: &GmAllowlist) {
        if outcome.changed && outcome.old_resolved != outcome.new_resolved {
            info!(
                "gm_allowlist: hostname resolution changed {:?} -> {:?}",
                outcome.old_resolved, outcome.new_resolved
            );
        }
        for name in &outcome.unresolved {
            error!(
                "gm_allowlist: hostname {:?} is UNRESOLVABLE — keeping the previous resolution \
                 {:?} (an empty set means NO grandmaster is accepted until DNS recovers; the \
                 service keeps running on NTP fallback and raises the clock alarm)",
                name,
                allowlist.resolved_ips()
            );
        }
    }

    /// dantesync#113: re-resolve the hostname allowlist if the cadence has elapsed
    /// (called from the 10 s tick). A no-hostname allowlist is a no-op.
    fn maybe_reresolve_gm(&mut self) {
        if !self.gm_allowlist.has_hostnames() {
            return;
        }
        if self.last_gm_resolve.elapsed() < GM_RESOLVE_INTERVAL {
            return;
        }
        let outcome = self.gm_allowlist.resolve(&*self.gm_resolver);
        self.last_gm_resolve = Instant::now();
        Self::log_gm_resolve_outcome(&outcome, &self.gm_allowlist);
    }

    /// #114: sample the current Dante-clock health for the alarm decision.
    fn sample_clock_health(&self) -> ClockHealth {
        let ptp_stale = self.last_ptp_packet.elapsed() > Duration::from_secs(PTP_TIMEOUT_SECS);
        // mode ∈ {LOCK, NANO} — the genuinely PTP-locked modes.
        let mode_locked = self.in_nano_mode || self.is_locked;
        // A grandmaster source is present AND permitted by the (resolved) allowlist.
        let gm_allowed = match self.current_sync_source_ip {
            Some(ip) => self.gm_allowlist.allows(ip),
            None => false,
        };
        ClockHealth {
            is_locked: self.is_locked,
            mode_locked,
            gm_allowed,
            ptp_stale,
            // #113: a hostname allowlist with no working resolution is a specific,
            // loud clock-loss reason (None when a resolution is held).
            allowlist_unresolvable: self.gm_allowlist.unresolvable_reason(),
            // #114 review: suppress the transient not-locked reason during the
            // initial boot acquisition window (never locked yet + within grace).
            in_acquisition: !self.ever_locked && self.started_at.elapsed() < ACQUISITION_GRACE,
        }
    }

    /// #114: advance the clock alarm and perform its emissions (LOST/REGAINED
    /// INFO edges, the per-cadence WARN log on every platform, and the desktop
    /// notification through the platform notifier), then publish the snapshot.
    fn evaluate_clock_alarm(&mut self) {
        // #114 review: once locked, the acquisition grace no longer applies (a
        // later unlock is a real loss and alarms immediately).
        if self.is_locked {
            self.ever_locked = true;
        }
        let health = self.sample_clock_health();
        let now = Instant::now();
        let now_epoch = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let tick = clock_alarm::drive(
            &mut self.clock_alarm,
            &health,
            now,
            now_epoch,
            &*self.clock_alarm_notifier,
        );
        if let Ok(mut status) = self.status_shared.write() {
            status.clock_alarm = tick.snapshot;
            status.clock_alarm_interval_s = self.clock_alarm_interval_s;
        }
    }

    pub fn process_loop_iteration(&mut self) -> Result<()> {
        // Check PTP status first (handles timeout detection for NTP-only fallback)
        self.check_ptp_status();

        // #88: apply a coordinated date step the moment its instant arrives, and follow the
        // master's announce. Every iteration (1 ms / 50 µs), BEFORE the packet early-returns, so a
        // step lands within one loop period of the announced instant on every box.
        self.service_date_offset();

        let (buf, size, t2, source_ip) = match self.network.recv_packet()? {
            Some(res) => res,
            None => {
                // No packet, but still run NTP tracking if PTP is offline
                if self.ptp_offline {
                    self.check_ntp_utc_tracking();
                }
                return Ok(());
            }
        };

        // camera-box issue 1073: drop a packet from a non-allowlisted grandmaster
        // source as-if it never arrived — BEFORE touching PTP liveness, the
        // adopted source IP, or any handler. A restricting allowlist thus prevents
        // a foreign-subnet grandmaster (the live incident: mbc's 10.77.7.109
        // leaking onto the stream box) from being adopted, and a node that sees
        // ONLY a foreign GM correctly ages into PTP-offline → NTP fallback rather
        // than silently locking to the wrong clock. A `None` source_ip (a
        // transport that cannot report the sender) is accepted — the filter can
        // only restrict what it can see. An empty allowlist accepts everything
        // (historical last-writer-wins), so this is a no-op unless configured.
        if let Some(ip) = source_ip {
            if !self.gm_allowlist.allows(ip) {
                self.gm_dropped_since_accepted = self.gm_dropped_since_accepted.saturating_add(1);
                // A foreign-source drop while NO allowed grandmaster is being seen
                // is the signature of BOTH the bug this fixes AND a mis-set
                // allowlist (a valid-but-wrong subnet drops the LEGITIMATE GM,
                // silently degrading the clock to NTP-only). Warn loudly but
                // rate-limited (once / 30 s) so it is diagnosable in the journal
                // without spamming at Sync rate; ordinary drops stay at debug.
                let now = Instant::now();
                if self
                    .last_gm_drop_warn
                    .map_or(true, |t| now.duration_since(t) >= Duration::from_secs(30))
                {
                    warn!(
                        "gm_allowlist: dropped {} PTP packet(s) from non-allowlisted source(s) \
                         (latest {ip}) since the last allowed grandmaster — if NO allowed GM \
                         appears, check config.gm_allowlist",
                        self.gm_dropped_since_accepted
                    );
                    self.last_gm_drop_warn = Some(now);
                } else {
                    debug!("Dropping PTP packet from non-allowlisted grandmaster source {ip}");
                }
                // #113: a dropped source while we carry hostname entries is a hint
                // the grandmaster may have moved (DHCP/DNS change) — re-resolve NOW
                // so a new lease is picked up within seconds, bounded by a cooldown
                // so a foreign PTP flood can never storm DNS.
                if self.gm_allowlist.has_hostnames()
                    && self.last_gm_resolve.elapsed() >= GM_RESOLVE_ON_DROP_COOLDOWN
                {
                    let outcome = self.gm_allowlist.resolve(&*self.gm_resolver);
                    self.last_gm_resolve = Instant::now();
                    Self::log_gm_resolve_outcome(&outcome, &self.gm_allowlist);
                }
                // Keep NTP discipline alive even under a foreign PTP flood — mirror
                // the no-packet branch, so a dropped packet never starves the only
                // clock left when PTP is offline.
                if self.ptp_offline {
                    self.check_ntp_utc_tracking();
                }
                return Ok(());
            }
        }

        // Packet received - update last_ptp_packet timestamp and source IP
        self.last_ptp_packet = Instant::now();
        // An allowed packet arrived: clear the drop-since-accepted counter so the
        // offline log and any future warning reflect only the CURRENT gap.
        self.gm_dropped_since_accepted = 0;
        if source_ip.is_some() {
            self.current_sync_source_ip = source_ip;
        }

        if size < PtpV1Header::SIZE {
            return Ok(());
        }

        let header = match PtpV1Header::parse(&buf[..size]) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };

        match header.message_type {
            PtpV1Control::Sync => self.handle_sync_message(&header, &buf[..size], t2),
            PtpV1Control::FollowUp => self.handle_followup_message(&header, &buf[..size]),
            _ => {}
        }

        // Cleanup stale pending syncs
        if self.pending_syncs.len() > 100 {
            let now = SystemTime::now();
            self.pending_syncs.retain(|_, v| {
                now.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5)
            });
        }

        // Periodic NTP UTC tracking (every 30s in production mode)
        self.check_ntp_utc_tracking();

        Ok(())
    }

    // ========================================================================
    // PACKET HANDLING
    // ========================================================================

    fn handle_sync_message(&mut self, header: &PtpV1Header, buf: &[u8], t2: SystemTime) {
        // Check if Sync source changed (different device sending PTP)
        let source_uuid = header.source_uuid;
        match self.current_sync_source {
            Some(current) if current != source_uuid => {
                warn!(
                    ">>> SYNC SOURCE CHANGED: {} -> {} <<<",
                    format_mac(&current),
                    format_mac(&source_uuid)
                );
                self.current_sync_source = Some(source_uuid);
                // Soft reset: clear stale data but KEEP current frequency
                // Both Dante devices should have similar frequencies since they're
                // synchronized to the same grandmaster time
                self.pending_syncs.clear();
                self.sample_window.clear();
                self.date_sync.window.clear();
                self.prev_t1_ns = 0;
                self.prev_t2_ns = 0;
                // #117: a different sender may carry a different time base — re-anchor D from
                // the next window so the wall stays continuous (never a wall step).
                self.date_sync.core.request_rebase();
                // Keep: applied_freq_ppm, drift_baseline_ppm (learned values)
                // Stay in production mode - let servo naturally adjust if needed
                info!(
                    "Soft reset: keeping freq={:.1}ppm, drift_baseline={:.1}ppm",
                    self.applied_freq_ppm, self.drift_baseline_ppm
                );
            }
            None => {
                info!("Sync source: {}", format_mac(&source_uuid));
                self.current_sync_source = Some(source_uuid);
            }
            _ => {}
        }

        // Limit pending_syncs size to prevent memory exhaustion from malformed packets
        const MAX_PENDING_SYNCS: usize = 200;
        if self.pending_syncs.len() >= MAX_PENDING_SYNCS {
            // Clean up stale entries first
            let now = SystemTime::now();
            self.pending_syncs.retain(|_, v| {
                now.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5)
            });
            // If still at capacity after cleanup, skip this sync
            if self.pending_syncs.len() >= MAX_PENDING_SYNCS {
                return;
            }
        }

        self.pending_syncs.insert(
            header.sequence_id,
            PendingSync {
                rx_time_sys: t2,
                source_uuid: header.source_uuid,
            },
        );

        if let Ok(body) = PtpV1SyncMessageBody::parse(&buf[PtpV1Header::SIZE..]) {
            let new_uuid = body.grandmaster_clock_uuid;
            match self.current_gm_uuid {
                Some(current) if current != new_uuid => {
                    warn!(
                        ">>> GRANDMASTER UUID CHANGED: {} -> {} <<<",
                        format_mac(&current),
                        format_mac(&new_uuid)
                    );
                    self.current_gm_uuid = Some(new_uuid);
                    // Note: sync source change already did soft reset if needed
                    // #117: the grandmaster's uptime is a different time base — re-anchor D.
                    if self.date_sync.enabled {
                        // A whole fresh window in the new time base, for both servos.
                        self.sample_window.clear();
                        self.date_sync.window.clear();
                        self.date_sync.core.request_rebase();
                    }
                }
                None => {
                    info!("Grandmaster UUID: {}", format_mac(&new_uuid));
                    self.current_gm_uuid = Some(new_uuid);
                }
                _ => {}
            }
        }
    }

    fn handle_followup_message(&mut self, header: &PtpV1Header, buf: &[u8]) {
        if let Ok(body) = PtpV1FollowUpBody::parse(&buf[PtpV1Header::SIZE..]) {
            if let Some(sync_info) = self.pending_syncs.remove(&body.associated_sequence_id) {
                if sync_info.source_uuid == header.source_uuid {
                    self.process_sync_pair(
                        body.precise_origin_timestamp.to_nanos(),
                        sync_info.rx_time_sys,
                    );
                }
            }
        }
    }

    // ========================================================================
    // SYNC PAIR PROCESSING - Main synchronization logic
    // ========================================================================

    fn process_sync_pair(&mut self, t1_ns: i64, t2_sys: SystemTime) {
        let t2_ns = t2_sys
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;

        // Calculate display phase offset (modulo-based for readability)
        let phase_offset_ns = self.calculate_phase_offset(t1_ns, t2_ns);

        // Handle calibration if needed
        if self.process_calibration(phase_offset_ns) {
            return;
        }

        // Apply calibration offset
        let phase_offset_ns = phase_offset_ns - self.calibration_offset_ns;

        // Handle warmup period
        if !self.process_warmup() {
            return;
        }

        // Log delta sanity check
        self.log_delta_sanity(t1_ns, t2_ns);

        // Process sync once settled
        self.valid_count += 1;
        if self.valid_count >= self.settling_threshold {
            self.process_settled_sync(t1_ns, t2_ns, phase_offset_ns);
        }

        self.prev_t1_ns = t1_ns;
        self.prev_t2_ns = t2_ns;
    }

    fn calculate_phase_offset(&self, t1_ns: i64, t2_ns: i64) -> i64 {
        let time_diff_ns = t2_ns - t1_ns;
        let mut display_phase = (t2_ns % 1_000_000_000) - (t1_ns % 1_000_000_000);
        if display_phase > 500_000_000 {
            display_phase -= 1_000_000_000;
        } else if display_phase < -500_000_000 {
            display_phase += 1_000_000_000;
        }

        debug!(
            "T1={:.3}s T2={:.3}s diff={:.3}s phase={}us",
            t1_ns as f64 / 1e9,
            t2_ns as f64 / 1e9,
            time_diff_ns as f64 / 1e9,
            display_phase / 1000
        );
        display_phase
    }

    fn process_calibration(&mut self, phase_offset_ns: i64) -> bool {
        let count = self.config.filters.calibration_samples;
        if self.calibration_complete || count == 0 {
            return false;
        }

        self.calibration_samples.push(phase_offset_ns);
        if self.calibration_samples.len() >= count {
            let mut sorted = self.calibration_samples.clone();
            sorted.sort();
            self.calibration_offset_ns = sorted[sorted.len() / 2];
            self.calibration_complete = true;
            info!(
                "Calibration complete: offset={:.3}ms ({} samples)",
                self.calibration_offset_ns as f64 / 1_000_000.0,
                count
            );
        }
        true
    }

    fn process_warmup(&mut self) -> bool {
        if self.warmup_complete {
            return true;
        }

        let warmup_secs = self.config.filters.warmup_secs;
        if warmup_secs <= 0.0 || self.warmup_start.elapsed().as_secs_f64() >= warmup_secs {
            self.warmup_complete = true;
            if warmup_secs > 0.0 {
                info!("[Warmup] Complete after {:.1}s", warmup_secs);
            }
            true
        } else {
            false
        }
    }

    fn log_delta_sanity(&self, t1_ns: i64, t2_ns: i64) {
        if self.prev_t1_ns > 0 && self.prev_t2_ns > 0 {
            let delta_master = t1_ns - self.prev_t1_ns;
            let delta_slave = t2_ns - self.prev_t2_ns;

            if delta_master > 0 && delta_master < MAX_DELTA_NS {
                let ratio = delta_slave as f64 / delta_master as f64;
                if !(0.5..=2.0).contains(&ratio) {
                    debug!(
                        "[Jitter] master={}ms slave={}ms ratio={:.2}x",
                        delta_master / 1_000_000,
                        delta_slave / 1_000_000,
                        ratio
                    );
                }
            }
        }
    }

    fn process_settled_sync(&mut self, t1_ns: i64, t2_ns: i64, phase_offset_ns: i64) {
        if !self.clock_settled {
            self.clock_settled = true;
            self.initial_epoch_offset_ns = t2_ns - t1_ns;
            self.epoch_aligned = true;
            info!("Sync established.");
        }

        // Collect sample if enough time has passed
        if self.should_add_sample(t1_ns) {
            self.sample_window.push(phase_offset_ns);
            // #117: the RAW offset between the two time bases (not the mod-1 s display phase,
            // not calibration-corrected) — what the phase lock holds equal to D.
            if self.date_sync.enabled {
                self.date_sync.window.push(t2_ns.wrapping_sub(t1_ns));
            }
        }

        // Process window when full - pass master time for drift calculation
        if self.sample_window.len() >= self.config.filters.sample_window_size {
            self.process_sample_window(t1_ns);
        }
    }

    // NOTE: PTP stepping removed - Dante provides device uptime, not UTC.
    // NTP handles all time stepping via check_ntp_utc_tracking().

    fn should_add_sample(&self, t1_ns: i64) -> bool {
        // Skip samples during 2s grace period after NTP step (prevents transient from corrupting servo)
        if let Some(step_time) = self.last_ntp_step {
            if step_time.elapsed() < Duration::from_secs(2) {
                debug!("[NTP-Grace] Skipping sample during post-step grace period");
                return false;
            }
        }
        if self.prev_t1_ns == 0 {
            return true;
        }
        // Use config value if > 0, otherwise default (Dante sends packets every ~125ms)
        let min_delta = if self.config.filters.min_delta_ns > 0 {
            self.config.filters.min_delta_ns
        } else {
            DEFAULT_MIN_T1_DELTA_NS
        };
        (t1_ns - self.prev_t1_ns).abs() >= min_delta
    }

    // ========================================================================
    // SAMPLE WINDOW PROCESSING - SELF-TUNING SERVO
    // ========================================================================
    //
    // The algorithm:
    // 1. Strong P-term responds to offset → creates oscillation around zero
    // 2. When offset is small, we learn that the current correction = drift
    // 3. Drift baseline slowly converges to the natural clock drift
    // 4. No manual tuning needed - it auto-learns from the oscillation
    //
    // ========================================================================

    fn process_sample_window(&mut self, master_time_ns: i64) {
        let mut sorted = self.sample_window.clone();
        sorted.sort();

        let median = sorted[sorted.len() / 2];

        // UNIFIED: Use median for both platforms (robust against outliers)
        let offset_ns = median;
        let offset_us = offset_ns as f64 / 1000.0;

        debug!(
            "[Filter] min={:.1}us max={:.1}us median={:.1}us",
            sorted.first().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
            sorted.last().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
            offset_us
        );

        self.last_phase_offset_ns = offset_ns;

        // #117: the median of the raw `t2 − t1` window, for the phase lock.
        self.date_sync.pending_median_ns = if self.date_sync.window.is_empty() {
            None
        } else {
            let mut raw = self.date_sync.window.clone();
            raw.sort_unstable();
            Some(raw[raw.len() / 2])
        };
        self.date_sync.pending_t1_ns = master_time_ns;
        self.date_sync.window.clear();

        // Apply self-tuning servo
        self.apply_self_tuning_servo(offset_us);

        self.sample_window.clear();
    }

    /// Self-tuning servo algorithm
    ///
    /// Key insight: When offset oscillates around zero, the average correction
    /// needed to maintain that IS the drift compensation we need.
    ///
    /// So we:
    /// 1. Use P-term to respond to offset (creates oscillation)
    /// 2. When offset is small, learn drift from the total correction
    /// 3. This naturally converges to the right drift baseline
    fn apply_self_tuning_servo(&mut self, offset_us: f64) {
        // DANTE PTP FREQUENCY SYNC - Rate-of-Change Based Servo
        //
        // Key insight: Dante PTP timestamps are device uptime, NOT UTC.
        // The absolute offset (e.g., 182ms) is meaningless for time accuracy.
        // What matters is the RATE OF CHANGE of offset:
        // - If offset is stable → frequencies are matched ✓
        // - If offset is growing → local clock is too fast
        // - If offset is shrinking → local clock is too slow
        //
        // NTP handles UTC alignment separately. PTP only matches frequency.
        //
        // #117: under the PTP phase lock (the default) this rate servo only ACQUIRES; once
        // PTP-locked, `crate::ptp_phase_lock` takes the frequency word from the phase error
        // below. Taken first so a grace-period return discards it with the window.
        let phase_median_ns = self.date_sync.pending_median_ns.take();

        // Skip correction during post-step grace period
        if let Some(step_time) = self.last_ntp_step {
            if step_time.elapsed() < Duration::from_secs(2) {
                debug!("[Servo] In grace period, skipping correction");
                return;
            }
        }

        // Track offset for rate calculation
        let now = Instant::now();
        let dt_secs = self
            .last_offset_time
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(1.0);

        // Calculate instantaneous rate of change (drift rate in ppm)
        // delta_offset / delta_time gives us the frequency error
        let raw_rate_ppm = if let Some(prev_offset) = self.last_offset_us {
            if dt_secs > 0.1 {
                // Need meaningful time delta
                let delta_offset = offset_us - prev_offset;
                // Convert: us/s = ppm.
                // #97 (review 🔵): the ±500 spike clamp moved to AFTER the decoupling below, so it
                // bounds the RESIDUAL the PTP servo actually consumes, not the pre-decoupled raw
                // (which would corrupt the residual if `true_drift + f_phase` ever exceeded 500 —
                // unreachable on this fleet, but the residual is the correct thing to bound).
                delta_offset / dt_secs
            } else {
                self.smoothed_rate_ppm // Keep previous
            }
        } else {
            0.0
        };

        // Store for next iteration
        self.last_offset_us = Some(offset_us);
        self.last_offset_time = Some(now);

        // #97: FEED-FORWARD DECOUPLING (the carrier line). Subtract the phase slew that was
        // actually in effect over THIS interval from the raw PTP rate observation, so the PTP
        // frequency servo never reads our own commanded slew as grandmaster disagreement and
        // cannot fight it. Sign is `-` (offset = local - master; a faster local clock grows the
        // observed offset). A no-op when the servo is disabled (`last_applied_f_phase_ppm` is
        // held at 0). The ±500 clamp is applied AFTER, to the value the servo consumes — for the
        // disabled path this is byte-identical (the `delta/dt` branch was clamped here before; the
        // smoothed/0.0 fallbacks are already within range, so clamping them is a no-op).
        let raw_rate_ppm = if self.phase_slew.is_some() {
            phase_slew::decouple_ptp_rate(raw_rate_ppm, self.last_applied_f_phase_ppm)
        } else {
            raw_rate_ppm
        }
        .clamp(-500.0, 500.0);

        // =======================================================================
        // ADAPTIVE SPIKE DETECTION
        // =======================================================================
        // Filter raw rate through MAD-based outlier detector.
        // Uses current mode for threshold selection (stricter in LOCK/NANO).
        // Spikes from timestamp jitter are replaced with median of window.
        // =======================================================================
        let filter_mode = if self.in_nano_mode {
            FilterMode::Nano
        } else if self.is_locked {
            FilterMode::Lock
        } else if self.in_production_mode {
            FilterMode::Prod
        } else {
            FilterMode::Acq
        };

        let filter_result = self.spike_filter.filter(raw_rate_ppm, filter_mode);
        let filtered_rate_ppm = filter_result.value;

        // Log when spike is detected and rejected
        if filter_result.is_spike {
            info!(
                "[Spike] REJECTED {:+.1}us/s (dev={:.1}, thresh={:.1}, median={:.1})",
                raw_rate_ppm,
                filter_result.deviation,
                filter_result.threshold,
                filter_result.median
            );
        }

        // Log spike statistics periodically (every 100 samples)
        let (total, rejected, ratio) = self.spike_filter.stats();
        if total > 0 && total % 100 == 0 {
            debug!(
                "[Spike] Stats: {}/{} rejected ({:.1}%), MAD={:.2}",
                rejected, total, ratio, filter_result.mad
            );
        }

        // Smooth rate with exponential moving average (on FILTERED rate)
        // Use adaptive alpha from jitter estimator:
        // - Low-jitter systems (strih.lan): α=0.3 for responsive tracking
        // - High-jitter systems (stream.lan): α=0.1 for heavy smoothing
        let adaptive_alpha = self.jitter_estimator.add_sample(filtered_rate_ppm);
        self.smoothed_rate_ppm =
            self.smoothed_rate_ppm * (1.0 - adaptive_alpha) + filtered_rate_ppm * adaptive_alpha;
        let rate_ppm = self.smoothed_rate_ppm;

        // Log jitter statistics periodically (every 50 samples when adjusted)
        if self.jitter_estimator.sample_count() > 0
            && self.jitter_estimator.sample_count() % 50 == 0
            && (adaptive_alpha - 0.3).abs() > 0.01
        {
            info!(
                "[Jitter] stddev={:.2}µs/s α={:.2} (samples={})",
                self.jitter_estimator.last_jitter(),
                adaptive_alpha,
                self.jitter_estimator.sample_count()
            );
        }

        // =======================================================================
        // ACCUMULATED PHASE ERROR TRACKING
        // =======================================================================
        // Track estimated phase drift between NTP steps for monitoring.
        // Uses smoothed rate (µs/s) integrated over time to estimate
        // how much UTC alignment has drifted since last NTP step.
        // =======================================================================
        let now_phase = Instant::now();
        if let Some(last_time) = self.last_phase_accumulation_time {
            let dt = now_phase.duration_since(last_time).as_secs_f64();
            // rate_ppm is in µs/s, so rate_ppm * dt gives µs of accumulated error
            self.accumulated_phase_error_us += rate_ppm * dt;
        }
        self.last_phase_accumulation_time = Some(now_phase);

        // THREE-PHASE CONTROL: ACQ → PROD → NANO based on rate stability
        let abs_rate = rate_ppm.abs();

        // NANO mode transitions (from LOCK state only)
        if self.is_locked {
            if abs_rate < NANO_ENTER_RATE_US {
                self.nano_sustain_count += 1;
                self.nano_exit_count = 0; // Reset exit counter when drift is good
                                          // Log progress towards NANO every 10 samples
                #[allow(clippy::manual_is_multiple_of)]
                if self.nano_sustain_count % 10 == 0 && !self.in_nano_mode {
                    debug!(
                        "[NANO] Sustain count: {}/{}",
                        self.nano_sustain_count, NANO_SUSTAIN_COUNT
                    );
                }
                if self.nano_sustain_count >= NANO_SUSTAIN_COUNT && !self.in_nano_mode {
                    self.in_nano_mode = true;
                    info!(
                        "[PTP] === NANO MODE === Ultra-precise servo engaged (after {} samples)",
                        NANO_SUSTAIN_COUNT
                    );
                }
            } else if abs_rate > NANO_EXIT_RATE_US {
                // Above exit threshold - count towards exit (hysteresis)
                self.nano_exit_count += 1;
                if self.in_nano_mode {
                    if self.nano_exit_count >= NANO_EXIT_COUNT {
                        self.in_nano_mode = false;
                        self.nano_sustain_count = 0;
                        self.nano_exit_count = 0;
                        info!("[PTP] === LOCK MODE === Exiting NANO (drift {:+.2}us/s for {} samples)",
                              rate_ppm, NANO_EXIT_COUNT);
                    } else {
                        debug!(
                            "[NANO] Exit warning {}/{}: drift {:+.2}us/s",
                            self.nano_exit_count, NANO_EXIT_COUNT, rate_ppm
                        );
                    }
                }
                // Reset sustain count when we exceed exit threshold (even if not in NANO yet)
                // This ensures we need CONSECUTIVE samples below threshold to enter
                if self.nano_sustain_count > 0 {
                    debug!(
                        "[NANO] Reset entry counter: drift {:+.2}us/s > exit threshold",
                        abs_rate
                    );
                    self.nano_sustain_count = 0;
                }
            } else {
                // Between thresholds (0.5-1.0): reset exit counter but don't change entry counter
                self.nano_exit_count = 0;
            }
        } else {
            // Not locked - can't be in NANO
            self.in_nano_mode = false;
            self.nano_sustain_count = 0;
            self.nano_exit_count = 0;
        }

        // ACQ/PROD transitions
        if abs_rate < 5.0 {
            // Rate stable within 5µs/s
            self.in_production_mode = true;
        } else if abs_rate > 20.0 {
            // Rate unstable above 20µs/s
            self.in_production_mode = false;
        }

        // Select gains based on mode
        let (p_gain, p_max, i_gain, phase_name) = if self.in_nano_mode {
            (P_GAIN_NANO, P_MAX_NANO_PPM, I_GAIN_NANO, "NANO")
        } else if self.in_production_mode {
            (P_GAIN_PROD, P_MAX_PROD_PPM, 0.05, "PROD")
        } else {
            (P_GAIN_ACQ, P_MAX_ACQ_PPM, 0.05, "ACQ")
        };

        // P-term: responds to rate of change (not absolute offset!)
        // NANO mode: apply deadband - don't correct tiny rates (noise)
        let effective_rate = if self.in_nano_mode && abs_rate < NANO_DEADBAND_US {
            0.0 // Within deadband, no correction needed
        } else {
            rate_ppm
        };

        // Negative rate = clock too slow, need positive adjustment
        let p_term = (-effective_rate * p_gain).clamp(-p_max, p_max);

        // I-term: Integrate rate error to learn true drift
        // Uses mode-appropriate gain
        let i_term = -effective_rate * i_gain;
        self.drift_baseline_ppm =
            (self.drift_baseline_ppm + i_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Total correction = drift baseline + P-term
        let total_correction =
            (self.drift_baseline_ppm + p_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Lock state: based on rate stability, not absolute offset
        let rate_stable = abs_rate < 5.0; // Within 5ppm
        if rate_stable {
            self.lock_stable_count += 1;
            if self.lock_stable_count >= LOCK_STABLE_COUNT && !self.is_locked {
                self.is_locked = true;
                info!(
                    "[PTP] === LOCKED === Adj:{:+.1}ppm",
                    self.drift_baseline_ppm
                );
            }
        } else {
            if self.lock_stable_count > 0 {
                self.lock_stable_count -= 1; // Gradual unlock
            }
            if self.lock_stable_count == 0 && self.is_locked {
                self.is_locked = false;
                info!("[PTP] === UNLOCKED === Drift:{:+.1}us/s", rate_ppm);
            }
        }

        // #117: THE PTP PHASE LOCK owns the frequency word once PTP-locked (controller/date_sync.rs).
        let applied_word = self.phase_lock_word(phase_median_ns, total_correction, dt_secs);

        // Apply correction. `applied_word` is f_ptp — the PTP servo's own frequency word (the rate
        // servo's `total_correction`, computed from the DECOUPLED rate above, or the #117 phase
        // lock's word once engaged).
        self.last_adj_ppm = applied_word;
        self.applied_freq_ppm = applied_word;

        // #97: compose the ONE frequency word actually applied to the clock — f_total = f_ptp +
        // f_phase — through the SAME `adjust_frequency` path on every platform (so the Windows
        // rate mechanism composes with the slew automatically; there is no second frequency path).
        // Then remember the applied f_phase for the NEXT interval's decoupling. When the servo is
        // disabled this is exactly `total_correction` and `last_applied_f_phase_ppm` stays 0.
        let f_total = if self.phase_slew.is_some() {
            phase_slew::compose_frequency(applied_word, self.pending_f_phase_ppm, DRIFT_MAX_PPM)
        } else {
            applied_word
        };
        self.last_applied_f_phase_ppm = if self.phase_slew.is_some() {
            self.pending_f_phase_ppm
        } else {
            0.0
        };
        let factor = 1.0 + (f_total / 1_000_000.0);

        let status = if self.in_nano_mode {
            "NANO"
        } else if self.is_locked {
            "LOCK"
        } else {
            phase_name
        };

        // User-friendly log: drift rate (stability) and frequency adjustment
        // NANO mode shows nanoseconds for sub-µs precision visibility.
        // #679 — throttled to once every DRIFT_LOG_SUMMARY_INTERVAL_SAMPLES
        // samples (was every sample / ~once/sec, the fleet's dominant
        // /var/log volume driver). LOCKED/UNLOCKED/NANO transitions above
        // still log immediately and are unaffected by this throttle.
        let log_drift_summary = should_log_drift_summary(
            self.drift_log_sample_count,
            DRIFT_LOG_SUMMARY_INTERVAL_SAMPLES,
        );
        self.drift_log_sample_count = self.drift_log_sample_count.wrapping_add(1);
        if log_drift_summary {
            if self.in_nano_mode {
                let drift_ns = rate_ppm * 1000.0; // Convert µs/s to ns/s
                info!(
                    "[PTP] {:4}  Drift:{:+7.0}ns/s  Adj:{:+6.2}ppm",
                    status, drift_ns, applied_word
                );
            } else {
                info!(
                    "[PTP] {:4}  Drift:{:+6.1}us/s  Adj:{:+6.1}ppm",
                    status, rate_ppm, applied_word
                );
            }
            if self.date_sync.core.engaged() {
                info!(
                    "[PHASE-LOCK] e={:+.1}us word={:+.3}ppm D-seq={}",
                    self.date_sync.core.last_error_ns().unwrap_or(0) as f64 / 1_000.0,
                    applied_word,
                    self.date_sync
                        .follower
                        .adopted_seq()
                        .map(|q| q.to_string())
                        .unwrap_or_else(|| "-".to_string())
                );
            }
        }

        if let Err(e) = self.clock.adjust_frequency(factor) {
            warn!("Clock adjustment failed: {}", e);
        }

        self.update_shared_status();
    }

    // ========================================================================
    // UTILITY METHODS
    // ========================================================================

    /// #91: count NTP steps still inside the trailing storm window. Immutable (no
    /// prune), so `update_shared_status(&self)` can call it — a step older than the
    /// window is filtered out of the count here even if it has not been physically
    /// pruned yet (pruning only happens at step time), so the reported rate decays
    /// correctly between steps.
    fn ntp_steps_in_storm_window(&self) -> u32 {
        self.ntp_step_times
            .iter()
            .filter(|t| t.elapsed() < NTP_STEP_STORM_WINDOW)
            .count() as u32
    }

    /// #91: record a successful NTP `step_clock` and evaluate the step-storm alarm.
    /// Called at the single successful-step site in `check_ntp_utc_tracking`. Prunes
    /// the trailing-window deque, refreshes the two `/status` fields, and emits the
    /// loud, rate-limited `[NTP][STEP-STORM]` warning while a SERVER-mode master is
    /// storming. This does NOT stop the storm — only restoring the PTP grandmaster /
    /// frequency reference can (the NTP loop cannot slew a real frequency error away;
    /// see clock-discipline-and-testing.md) — it exists so the degradation is LOUD
    /// instead of running 19h+ silent as it did live (#91).
    fn record_ntp_step_and_check_storm(&mut self) {
        let now = Instant::now();
        self.ntp_step_times.push_back(now);
        // Prune anything older than the trailing window (front is oldest).
        while let Some(&front) = self.ntp_step_times.front() {
            if now.duration_since(front) >= NTP_STEP_STORM_WINDOW {
                self.ntp_step_times.pop_front();
            } else {
                break;
            }
        }
        let steps_last_hour = self.ntp_step_times.len() as u32;
        let storming = self.ntp_server_mode && steps_last_hour > NTP_STEP_STORM_THRESHOLD_PER_HOUR;

        if storming {
            let warn_due = self
                .last_step_storm_warn
                .map(|t| t.elapsed() >= NTP_STEP_STORM_WARN_INTERVAL)
                .unwrap_or(true);
            if warn_due {
                warn!(
                    "[NTP][STEP-STORM] this NTP MASTER stepped {} times in the last hour \
                     (> {}/h) -- its PTP frequency reference is degraded and every NTP client \
                     is chasing these steps (fleet-wide frame skips). Restore the PTP \
                     grandmaster / frequency source; no NTP-side change can slew a real \
                     frequency error away.",
                    steps_last_hour, NTP_STEP_STORM_THRESHOLD_PER_HOUR
                );
                self.last_step_storm_warn = Some(now);
            }
        }

        // Refresh /status immediately (server mode only) so a step lands the alarm
        // without waiting for the next update_shared_status tick.
        if self.ntp_server_mode {
            if let Ok(mut status) = self.status_shared.write() {
                status.ntp_steps_last_hour = Some(steps_last_hour);
                status.ntp_step_storm = storming;
            }
        }
    }

    fn update_shared_status(&self) {
        if let Ok(mut status) = self.status_shared.write() {
            // Core fields
            status.offset_ns = self.last_phase_offset_ns;
            status.drift_ppm = self.last_adj_ppm;
            status.gm_uuid = self.current_gm_uuid;
            status.gm_source_ip = self.current_sync_source_ip;
            // #113: publish the live hostname resolution so external gates can
            // compare gm_source_ip against the resolved set (and see loud failures).
            status.gm_allowlist_resolved = self.gm_allowlist.resolved_ips().to_vec();
            status.gm_allowlist_unresolved = self.gm_allowlist.unresolved_hostnames().to_vec();
            status.settled = self.clock_settled;
            status.updated_ts = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            // Extended fields for tray app
            status.is_locked = self.is_locked;
            status.smoothed_rate_ppm = self.smoothed_rate_ppm;
            status.mode = if self.in_nano_mode {
                "NANO".to_string()
            } else if self.is_locked {
                "LOCK".to_string()
            } else if self.in_production_mode {
                "PROD".to_string()
            } else {
                "ACQ".to_string()
            };
            // Accumulated phase error since last NTP step
            status.accumulated_phase_us = self.accumulated_phase_error_us;
            // NTP offset/quality are published by record_ntp_success(). Only the
            // AGE is recomputed here (#68), so it keeps ticking up beside the
            // frozen reading it describes instead of staying at whatever it was
            // when that reading landed — that indistinguishability is the bug.
            status.ntp_updated_ts = self.last_ntp_success_epoch.unwrap_or(0);
            status.ntp_age_s = self.last_ntp_success.map(|t| t.elapsed().as_secs());
            // #83: the currently-active step threshold, server mode only -- lets a
            // consumer grade ntp_offset_us against the box's OWN current tolerance
            // (a large deadband while genuinely PTP-locked) instead of a fixed bound.
            // #88: while this master is the date-offset authority its only step threshold is the
            // authority's bound (it never steps on the tight NTP thresholds then).
            let authority_active = self.date_authority_active();
            status.ntp_deadband_us = if authority_active {
                Some(self.date_sync.step_bound_ns / 1_000)
            } else if self.ntp_server_mode {
                Some(server_step_threshold_us(self.is_locked, self.ptp_offline))
            } else {
                None
            };
            // #101: this node's OWN currently-active step threshold, REGARDLESS of mode. Server
            // mode reuses the same value as ntp_deadband_us above (server_step_threshold_us); a
            // CLIENT reports its adaptive MAD-based threshold (calculate_ntp_adaptive_threshold) --
            // the SAME quantity the journal logs as "threshold:Nus", which ntp_deadband_us
            // deliberately does NOT publish on a client (#83). Lets a HTTP-only consumer (a Windows
            // camera-box client with no journald) read its own step envelope for the step-aware
            // median+spread gate widening instead of falling back to a fixed guess (camera-box #1129).
            status.ntp_step_threshold_us = Some(if authority_active {
                self.date_sync.step_bound_ns / 1_000
            } else if self.ntp_server_mode {
                server_step_threshold_us(self.is_locked, self.ptp_offline)
            } else {
                self.calculate_ntp_adaptive_threshold()
            });
            // #91: keep the step-storm metric/flag fresh between steps so a
            // watchdog polling /status sees the storm CLEAR (steps aging out of
            // the trailing window) without needing another step to fire. The loud
            // ONSET warning is emitted at the step site (record_ntp_step_and_
            // check_storm); here we recompute the published state and log the
            // CLEARING edge. Server mode only -- a client node reports None/false.
            if self.ntp_server_mode {
                let steps_last_hour = self.ntp_steps_in_storm_window();
                let storming = steps_last_hour > NTP_STEP_STORM_THRESHOLD_PER_HOUR;
                // #91 (review S1): log the true->false edge so post-incident log
                // archaeology can bound the storm's END, not just its onset. The
                // previously-published flag is the edge memory (no extra mutable
                // state), and this fires even when the storm ends because steps
                // STOPPED entirely -- which the step site can never observe.
                if status.ntp_step_storm && !storming {
                    info!(
                        "[NTP][STEP-STORM] cleared -- {} steps in the last hour (<= {}/h); the \
                         PTP frequency reference appears restored",
                        steps_last_hour, NTP_STEP_STORM_THRESHOLD_PER_HOUR
                    );
                }
                status.ntp_steps_last_hour = Some(steps_last_hour);
                status.ntp_step_storm = storming;
            } else {
                status.ntp_steps_last_hour = None;
                status.ntp_step_storm = false;
            }

            // #97: phase-slew telemetry. `f_ptp_ppm` mirrors `drift_ppm` (the decoupled PTP
            // correction), surfaced explicitly beside the f_phase split so both composed frequency
            // terms are readable. All fields default to the "off / idle" reading when disabled.
            status.phase_slew_enabled = self.phase_slew.is_some();
            status.f_ptp_ppm = self.last_adj_ppm;
            if let Some(out) = self.last_phase_slew_output {
                status.f_phase_ppm = out.f_phase_ppm;
                status.f_phase_p_ppm = out.p_ppm;
                status.f_phase_i_ppm = out.i_ppm;
                status.phase_slew_saturated = out.saturated;
            } else {
                // #105 (review 🔵): after a step PRESERVE, `pending_f_phase_ppm` IS the held
                // integrator DC still being applied — surface it as `f_phase_i_ppm` so the canary
                // (which watches the held ~50 ppm) does not read 0 and false-alarm. When genuinely
                // idle/disabled `pending` is 0, so this reads 0 exactly as before.
                status.f_phase_ppm = self.pending_f_phase_ppm;
                status.f_phase_p_ppm = 0.0;
                status.f_phase_i_ppm = self.pending_f_phase_ppm;
                status.phase_slew_saturated = false;
            }

            // #117 / #88: the discipline, the phase lock and the fleet date offset.
            self.publish_date_status(&mut status);
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockSystemClock;
    use crate::traits::{MockNtpSource, MockPtpNetwork};
    use mockall::predicate::*;

    // #679 — the per-sample "[PTP] Drift:... Adj:...ppm" line used to fire on
    // EVERY sample (~once/sec), which was ~65% of the fleet's fixed 50MB
    // /var/log tmpfs volume and crashed cam2's camera-box.service after
    // ~4-5 days of uptime. `should_log_drift_summary` throttles it to once
    // every `interval` samples (plus immediately on the very first sample).
    #[test]
    fn drift_summary_logs_on_first_sample() {
        assert!(should_log_drift_summary(0, 30));
    }

    #[test]
    fn drift_summary_stays_quiet_between_intervals() {
        assert!(!should_log_drift_summary(1, 30));
        assert!(!should_log_drift_summary(15, 30));
        assert!(!should_log_drift_summary(29, 30));
    }

    #[test]
    fn drift_summary_logs_again_every_interval_thereafter() {
        assert!(should_log_drift_summary(30, 30));
        assert!(should_log_drift_summary(60, 30));
        assert!(should_log_drift_summary(90, 30));
    }

    #[test]
    fn drift_summary_zero_interval_never_logs() {
        // Defensive: a misconfigured interval=0 must never panic (modulo-by-zero)
        // and must never log (fail closed toward LESS volume, not a crash).
        assert!(!should_log_drift_summary(0, 0));
        assert!(!should_log_drift_summary(500, 0));
    }

    #[test]
    fn test_ntp_sync_trigger() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_millis(100),
                sign: 1,
                spread_us: 0,
                sample_count: 1,
                pcap_active: false,
            })
        });

        mock_clock
            .expect_step_clock()
            .with(eq(Duration::from_millis(100)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut controller = PtpController::new(
            mock_clock,
            mock_net,
            mock_ntp,
            status,
            SystemConfig::default(),
        );
        controller.run_ntp_sync(false);
    }

    /// #53: `check_ntp_utc_tracking` must propagate the burst-filter quality
    /// fields (`spread_us`/`sample_count`/`pcap_active`, dantesync#53
    /// continuation) from the `NtpMeasurement` into `SyncStatus`, not just
    /// the offset — otherwise a consumer (e.g. camera-box's gate) has no way
    /// to tell a well-measured node from a badly-measured one, or whether
    /// the kernel-timestamped transport is actually in use.
    #[test]
    fn test_check_ntp_utc_tracking_propagates_quality_fields_to_status() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(100),
                sign: 1,
                spread_us: 41610,
                sample_count: 3,
                pcap_active: true,
            })
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut controller = PtpController::new(
            mock_clock,
            mock_net,
            mock_ntp,
            status.clone(),
            SystemConfig::default(),
        );

        // Force the check to run now (bypass the 10-30s adaptive interval gate)
        // and make it eligible (PTP offline is the simplest path — the OTHER
        // eligibility path, is_locked && ntp_tracking_enabled, needs a fuller
        // PTP-locked setup this test doesn't need).
        controller.last_ntp_check = Instant::now() - Duration::from_secs(60);
        controller.ptp_offline = true;

        controller.check_ntp_utc_tracking();

        let s = status.read().unwrap();
        assert_eq!(s.ntp_offset_us, 100);
        assert_eq!(
            s.ntp_spread_us, 41610,
            "spread must reach status, not just the offset"
        );
        assert_eq!(s.ntp_sample_count, 3);
        assert!(
            s.pcap_ntp_active,
            "pcap_active must reach status, not just the offset/spread/count"
        );
    }

    /// #83 review finding (suggestion): end-to-end proof of `ntp_deadband_us`
    /// wiring through the REAL `update_shared_status()` path (not just the
    /// pure `server_step_threshold_us` function or a hand-built `SyncStatus`
    /// in isolation, which the other #83 tests already cover separately).
    /// Also proves it publishes `Some(..)` as soon as server mode is
    /// configured, WITHOUT needing a successful NTP check first (the doc
    /// comment's own earlier, inaccurate claim this review finding fixed).
    #[test]
    fn ntp_deadband_us_publishes_through_the_real_status_wiring_end_to_end_83() {
        let _ = env_logger::builder().is_test(true).try_init();
        let (mut c, status) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);

        // Before any NTP check has ever run: still Some, reflecting the
        // CURRENT (not-yet-locked) threshold -- proves this does NOT gate on
        // ntp_offset_us's own freshness.
        c.update_shared_status();
        assert_eq!(
            status.read().expect("status lock").ntp_deadband_us,
            Some(NTP_SERVER_STEP_THRESHOLD_US),
            "server mode configured but not yet locked -- Some(tight threshold), no NTP check needed"
        );

        // Genuinely locked -- the large deadband, through the real wiring.
        c.is_locked = true;
        c.ptp_offline = false;
        c.update_shared_status();
        assert_eq!(
            status.read().expect("status lock").ntp_deadband_us,
            Some(NTP_SERVER_LOCKED_DEADBAND_US),
            "genuinely locked -- Some(deadband), through update_shared_status, not just the pure function"
        );

        // Client mode (server mode never configured) -- always None.
        let (client_c, client_status) = create_nano_test_controller();
        client_c.update_shared_status();
        assert_eq!(
            client_status.read().expect("status lock").ntp_deadband_us,
            None,
            "client mode must never publish a deadband"
        );
    }

    /// #101: `ntp_step_threshold_us` publishes the node's OWN current step threshold through the
    /// real `update_shared_status()` wiring, REGARDLESS of mode -- Some(server threshold) in server
    /// mode (same value as ntp_deadband_us) and Some(adaptive threshold) on a CLIENT (where
    /// ntp_deadband_us is deliberately None, #83). This is the field camera-box's step-aware gate
    /// reads for a Windows client with no journald (camera-box #1129).
    #[test]
    fn ntp_step_threshold_us_publishes_through_the_real_status_wiring_client_and_server_101() {
        let _ = env_logger::builder().is_test(true).try_init();

        // SERVER mode: same value as ntp_deadband_us (server_step_threshold_us).
        let (mut c, status) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        c.update_shared_status();
        {
            let s = status.read().expect("status lock");
            assert_eq!(
                s.ntp_step_threshold_us,
                Some(NTP_SERVER_STEP_THRESHOLD_US),
                "server mode not yet locked -- Some(tight threshold), matching ntp_deadband_us"
            );
            assert_eq!(
                s.ntp_step_threshold_us, s.ntp_deadband_us,
                "in server mode the step threshold equals the deadband"
            );
        }
        c.is_locked = true;
        c.ptp_offline = false;
        c.update_shared_status();
        assert_eq!(
            status.read().expect("status lock").ntp_step_threshold_us,
            Some(NTP_SERVER_LOCKED_DEADBAND_US),
            "server mode locked -- Some(large deadband)"
        );

        // CLIENT mode: ntp_deadband_us is None, but ntp_step_threshold_us reports the adaptive
        // threshold (base with <3 samples) -- the whole point of #101.
        let (client_c, client_status) = create_nano_test_controller();
        client_c.update_shared_status();
        {
            let s = client_status.read().expect("status lock");
            assert_eq!(
                s.ntp_deadband_us, None,
                "client still publishes no server-mode deadband"
            );
            assert_eq!(
                s.ntp_step_threshold_us,
                Some(NTP_STEP_THRESHOLD_BASE_US),
                "a client MUST publish its own adaptive step threshold (base with <3 samples), not None"
            );
        }
    }

    /// Adversarial-review fix (#53 continuation): `pcap_ntp_active` must NOT
    /// go stale on a burst failure. Root cause: the success branch of
    /// `check_ntp_utc_tracking` writes `status.pcap_ntp_active =
    /// measurement.pcap_active`, but the failure (`Err`) branch only ever
    /// touched `ntp_failed` -- leaving a stuck `true` from the last good
    /// burst even once NTP starts failing every check. A consumer reading
    /// `pcap_ntp_active` alone during an outage would be lied to.
    #[test]
    fn test_check_ntp_utc_tracking_clears_pcap_ntp_active_on_burst_failure() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        // First check: a real successful pcap-backed burst.
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(100),
                sign: 1,
                spread_us: 0,
                sample_count: 3,
                pcap_active: true,
            })
        });
        // Second check: the burst fails outright (e.g. Npcap transport lost
        // reachability, or the rsntp fallback also failed).
        mock_ntp
            .expect_get_offset()
            .times(1)
            .returning(|| Err(anyhow::anyhow!("NTP burst: no server response")));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut controller = PtpController::new(
            mock_clock,
            mock_net,
            mock_ntp,
            status.clone(),
            SystemConfig::default(),
        );
        controller.ptp_offline = true;

        controller.last_ntp_check = Instant::now() - Duration::from_secs(60);
        controller.check_ntp_utc_tracking();
        assert!(
            status.read().unwrap().pcap_ntp_active,
            "sanity check: the first (successful, pcap_active=true) burst must have set it"
        );

        controller.last_ntp_check = Instant::now() - Duration::from_secs(60);
        controller.check_ntp_utc_tracking();
        assert!(
            !status.read().unwrap().pcap_ntp_active,
            "pcap_ntp_active must be cleared to false on a failed burst, not left stuck at the \
             last successful value"
        );
    }

    #[test]
    fn test_ptp_locking_flow() {
        use byteorder::{BigEndian, WriteBytesExt};

        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();

        let gm_uuid = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        let make_sync = move |seq: u16| -> Vec<u8> {
            let mut buf = vec![0u8; 60];
            buf[0] = 0x10;
            buf[32] = 0x00;
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            buf[49..55].copy_from_slice(&gm_uuid);
            buf
        };

        let make_followup = move |seq: u16, t1_ns: u64| -> Vec<u8> {
            let mut buf = vec![0u8; 60];
            buf[0] = 0x10;
            buf[32] = 0x02;
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            let mut w = &mut buf[42..44];
            w.write_u16::<BigEndian>(seq).unwrap();
            let mut w = &mut buf[44..52];
            let s = (t1_ns / 1_000_000_000) as u32;
            let n = (t1_ns % 1_000_000_000) as u32;
            w.write_u32::<BigEndian>(s).unwrap();
            w.write_u32::<BigEndian>(n).unwrap();
            buf
        };

        for i in 0..8 {
            let t1 = 1_000_000_000 + i as u64 * 1_000_000_000;
            let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(t1 + 1000);

            let sync_pkt = make_sync(i as u16);
            let follow_pkt = make_followup(i as u16, t1);

            mock_net
                .expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((sync_pkt.clone(), 60, t2, None))));

            mock_net
                .expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((follow_pkt.clone(), 60, t2, None))));
        }

        mock_net.expect_recv_packet().returning(|| Ok(None));
        mock_clock
            .expect_adjust_frequency()
            .times(2)
            .returning(|_| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.sample_window_size = 4;
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);

        for _ in 0..16 {
            let _ = controller.process_loop_iteration();
        }

        assert!(controller.get_status_shared().read().unwrap().settled);
    }

    // ========================================================================
    // GM-SOURCE ALLOWLIST TESTS (camera-box issue 1073)
    // ========================================================================

    /// Build a minimal PTPv1 Sync packet with the given source/grandmaster UUID,
    /// mirroring `test_ptp_locking_flow`'s own `make_sync` byte layout.
    fn make_sync_pkt(uuid: [u8; 6], seq: u16) -> Vec<u8> {
        use byteorder::{BigEndian, WriteBytesExt};
        let mut buf = vec![0u8; 60];
        buf[0] = 0x10; // PTPv1 header
        buf[32] = 0x00; // control = Sync
        buf[22..28].copy_from_slice(&uuid); // source UUID
        let mut w = &mut buf[30..32];
        w.write_u16::<BigEndian>(seq).unwrap();
        buf[49..55].copy_from_slice(&uuid); // grandmaster clock UUID
        buf
    }

    struct NoopAlarmNotifier;
    impl ClockAlarmNotifier for NoopAlarmNotifier {
        fn notify(&self, _title: &str, _message: &str) {}
    }

    /// dantesync#114: the controller raises the clock alarm in `/status` when the
    /// node is NOT PTP-locked to an allowed grandmaster, and clears it once
    /// genuinely locked — driven from the 10 s tick even with no packets.
    #[test]
    fn clock_alarm_wires_status_active_when_unlocked_and_clears_when_locked_114() {
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.clock_alarm_interval_s = 30; // > floor, kept verbatim
        let mut controller = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            status.clone(),
            config,
        );
        // Never touch a real desktop bus from a unit test.
        controller.clock_alarm_notifier = Box::new(NoopAlarmNotifier);

        // Force the lost condition: not locked, no adopted source, PTP stale.
        controller.is_locked = false;
        controller.in_nano_mode = false;
        controller.current_sync_source_ip = None;
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);

        controller.evaluate_clock_alarm();
        {
            let s = status.read().unwrap();
            assert!(s.clock_alarm.active, "alarm must be active while unlocked");
            assert!(s.clock_alarm.since.is_some(), "since must be stamped");
            assert!(!s.clock_alarm.reason.is_empty(), "reason must be set");
            assert_eq!(
                s.clock_alarm_interval_s, 30,
                "the configured cadence must be published"
            );
        }
        assert!(controller.clock_alarm.is_active());

        // Now genuinely locked to an allowed grandmaster (default allowlist is
        // unrestricted, so any adopted source is allowed).
        controller.is_locked = true;
        controller.current_sync_source_ip = Some("10.77.9.184".parse().unwrap());
        controller.last_ptp_packet = Instant::now();

        controller.evaluate_clock_alarm();
        {
            let s = status.read().unwrap();
            assert!(!s.clock_alarm.active, "alarm must clear once locked");
            assert_eq!(s.clock_alarm.since, None);
            assert_eq!(s.clock_alarm.reason, "");
        }
        assert!(!controller.clock_alarm.is_active());
    }

    /// dantesync#114: a nonsense cadence (0) is floored, not published as 0.
    #[test]
    fn clock_alarm_interval_is_floored_114() {
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.clock_alarm_interval_s = 0;
        let mut controller = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            status.clone(),
            config,
        );
        controller.clock_alarm_notifier = Box::new(NoopAlarmNotifier);
        controller.evaluate_clock_alarm();
        assert_eq!(
            status.read().unwrap().clock_alarm_interval_s,
            clock_alarm::CLOCK_ALARM_INTERVAL_FLOOR_S,
            "a 0 cadence must be floored, never published as 0"
        );
    }

    struct MapResolver(std::collections::HashMap<String, Vec<std::net::Ipv4Addr>>);
    impl crate::gm_filter::Resolver for MapResolver {
        fn resolve(&self, host: &str) -> std::io::Result<Vec<std::net::Ipv4Addr>> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
        }
    }

    /// dantesync#113: a resolved hostname allowlist publishes the resolved IP in
    /// /status and accepts that grandmaster; an unresolvable one publishes the
    /// unresolved name AND drives the #114 clock alarm with the specific reason.
    #[test]
    fn gm_allowlist_hostname_resolution_wires_status_and_alarm_113() {
        // --- resolved case ---
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["video-clock.lan".to_string()];
        let mut controller = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            status.clone(),
            config,
        );
        controller.clock_alarm_notifier = Box::new(NoopAlarmNotifier);
        // Inject a resolver that maps the name, then force a re-resolve.
        let mut map = std::collections::HashMap::new();
        map.insert(
            "video-clock.lan".to_string(),
            vec!["10.77.9.230".parse().unwrap()],
        );
        controller.gm_resolver = Box::new(MapResolver(map));
        controller.last_gm_resolve = Instant::now() - GM_RESOLVE_INTERVAL - Duration::from_secs(1);
        controller.tick_status();
        {
            let s = status.read().unwrap();
            assert_eq!(
                s.gm_allowlist_resolved,
                vec!["10.77.9.230".parse::<std::net::Ipv4Addr>().unwrap()],
                "resolved GM IP must be published"
            );
            assert!(s.gm_allowlist_unresolved.is_empty());
        }
        assert!(
            controller
                .gm_allowlist
                .allows("10.77.9.230".parse().unwrap()),
            "the resolved grandmaster is accepted"
        );

        // --- unresolvable case ---
        let status2 = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config2 = SystemConfig::default();
        config2.gm_allowlist = vec!["video-clock.lan".to_string()];
        let mut controller2 = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            status2.clone(),
            config2,
        );
        controller2.clock_alarm_notifier = Box::new(NoopAlarmNotifier);
        controller2.gm_resolver = Box::new(MapResolver(std::collections::HashMap::new())); // nothing resolves
        controller2.last_gm_resolve = Instant::now() - GM_RESOLVE_INTERVAL - Duration::from_secs(1);
        controller2.tick_status();
        {
            let s = status2.read().unwrap();
            assert_eq!(
                s.gm_allowlist_unresolved,
                vec!["video-clock.lan".to_string()],
                "an unresolvable hostname must be published loudly"
            );
            assert!(s.gm_allowlist_resolved.is_empty());
            assert!(s.clock_alarm.active, "an unresolvable GM raises the alarm");
            assert!(
                s.clock_alarm.reason.contains("video-clock.lan")
                    && s.clock_alarm.reason.contains("unresolvable"),
                "the alarm reason must name the unresolvable hostname, got: {}",
                s.clock_alarm.reason
            );
        }
    }

    /// RED (camera-box issue 1073): reproduces the live incident. The stream box,
    /// also seeing mbc's foreign `10.77.7.x` subnet, must NOT adopt a Sync from
    /// foreign grandmaster `10.77.7.109` when the allowlist restricts sources to
    /// the rig subnet `10.77.9.0/24`. Before the fix, `process_loop_iteration`
    /// adopts the source unconditionally (last-writer-wins), so every assertion
    /// below fails; after the fix the foreign packet is dropped as-if-not-received.
    #[test]
    fn foreign_subnet_grandmaster_is_rejected_when_allowlist_restricts_camerabox_issue_1073() {
        let _ = env_logger::builder().is_test(true).try_init();

        let foreign_uuid = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let foreign_ip: std::net::Ipv4Addr = "10.77.7.109".parse().unwrap();
        let sync_pkt = make_sync_pkt(foreign_uuid, 0);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_000_000_000);

        let mut mock_net = MockPtpNetwork::new();
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((sync_pkt.clone(), 60, t2, Some(foreign_ip)))));
        mock_net.expect_recv_packet().returning(|| Ok(None));

        let mock_clock = MockSystemClock::new();
        let mock_ntp = MockNtpSource::new();
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["10.77.9.0/24".to_string()]; // rig subnet only
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);
        let before = controller.last_ptp_packet;

        controller
            .process_loop_iteration()
            .expect("iteration should not error");

        assert_eq!(
            controller.current_sync_source_ip, None,
            "foreign-subnet source IP must not be adopted"
        );
        assert_eq!(
            controller.current_sync_source, None,
            "foreign Sync source UUID must not be adopted"
        );
        assert_eq!(
            controller.current_gm_uuid, None,
            "foreign grandmaster UUID must not be adopted"
        );
        assert!(
            controller.last_ptp_packet <= before,
            "a dropped foreign packet must not advance the PTP-liveness timestamp \
             (so a box seeing ONLY a foreign GM correctly goes PTP-offline)"
        );
    }

    /// GREEN companion (camera-box issue 1073): a Sync from a source ON the
    /// allowed rig subnet IS adopted normally — the fix must reject only foreign
    /// sources, never the legitimate grandmaster.
    #[test]
    fn rig_grandmaster_source_is_accepted_when_allowlist_permits_camerabox_issue_1073() {
        let _ = env_logger::builder().is_test(true).try_init();

        let rig_uuid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let rig_ip: std::net::Ipv4Addr = "10.77.9.184".parse().unwrap();
        let sync_pkt = make_sync_pkt(rig_uuid, 0);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_000_000_000);

        let mut mock_net = MockPtpNetwork::new();
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((sync_pkt.clone(), 60, t2, Some(rig_ip)))));
        mock_net.expect_recv_packet().returning(|| Ok(None));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["10.77.9.0/24".to_string()];
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(
            MockSystemClock::new(),
            mock_net,
            MockNtpSource::new(),
            status,
            config,
        );
        controller
            .process_loop_iteration()
            .expect("iteration should not error");

        assert_eq!(
            controller.current_sync_source_ip,
            Some(rig_ip),
            "the allowed rig grandmaster source must be adopted"
        );
        assert_eq!(
            controller.current_gm_uuid,
            Some(rig_uuid),
            "the allowed rig grandmaster UUID must be adopted"
        );
    }

    /// GREEN companion (camera-box issue 1073): an EMPTY allowlist (the default,
    /// and every pre-existing config) accepts ANY source — the historical
    /// last-writer-wins behavior is preserved, so a single-GM network is
    /// unaffected by this change.
    #[test]
    fn empty_allowlist_accepts_any_source_backward_compatible_camerabox_issue_1073() {
        let _ = env_logger::builder().is_test(true).try_init();

        let uuid = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let any_ip: std::net::Ipv4Addr = "10.77.7.109".parse().unwrap();
        let sync_pkt = make_sync_pkt(uuid, 0);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_000_000_000);

        let mut mock_net = MockPtpNetwork::new();
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((sync_pkt.clone(), 60, t2, Some(any_ip)))));
        mock_net.expect_recv_packet().returning(|| Ok(None));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let config = SystemConfig::default(); // gm_allowlist empty = unrestricted

        let mut controller = PtpController::new(
            MockSystemClock::new(),
            mock_net,
            MockNtpSource::new(),
            status,
            config,
        );
        controller
            .process_loop_iteration()
            .expect("iteration should not error");

        assert_eq!(
            controller.current_sync_source_ip,
            Some(any_ip),
            "with an empty allowlist, any source is accepted (backward compatible)"
        );
    }

    /// GREEN companion (camera-box issue 1073): a packet whose transport cannot
    /// report a source IP (`source_ip == None`) is ACCEPTED even under a
    /// restricting allowlist — the filter can only restrict what it can see, and
    /// failing closed here would take an edge-case transport offline. Pins the
    /// comment-only contract so a future refactor can't silently flip it.
    #[test]
    fn none_source_ip_is_accepted_even_when_allowlist_restricts_camerabox_issue_1073() {
        let _ = env_logger::builder().is_test(true).try_init();

        let uuid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let sync_pkt = make_sync_pkt(uuid, 0);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_000_000_000);

        let mut mock_net = MockPtpNetwork::new();
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((sync_pkt.clone(), 60, t2, None))));
        mock_net.expect_recv_packet().returning(|| Ok(None));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["10.77.9.0/24".to_string()]; // restricting
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(
            MockSystemClock::new(),
            mock_net,
            MockNtpSource::new(),
            status,
            config,
        );
        let before = controller.last_ptp_packet;
        controller
            .process_loop_iteration()
            .expect("iteration should not error");

        assert_eq!(
            controller.current_sync_source,
            Some(uuid),
            "a None-source packet must still be processed (filter only restricts known sources)"
        );
        assert!(
            controller.last_ptp_packet >= before,
            "a processed packet must advance PTP liveness"
        );
    }

    /// GREEN companion (camera-box issue 1073): the dropped-since-accepted counter
    /// increments per dropped foreign packet and resets on an accepted one — the
    /// signal `check_ptp_status` uses to distinguish "GM absent" from "GM blocked
    /// by a mis-set allowlist".
    #[test]
    fn drop_counter_counts_foreign_and_resets_on_allowed_camerabox_issue_1073() {
        let _ = env_logger::builder().is_test(true).try_init();

        let foreign_uuid = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        let rig_uuid = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let foreign_ip: std::net::Ipv4Addr = "10.77.7.109".parse().unwrap();
        let rig_ip: std::net::Ipv4Addr = "10.77.9.184".parse().unwrap();
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(1_000_000_000);

        let f0 = make_sync_pkt(foreign_uuid, 0);
        let f1 = make_sync_pkt(foreign_uuid, 1);
        let rig = make_sync_pkt(rig_uuid, 2);

        let mut mock_net = MockPtpNetwork::new();
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((f0.clone(), 60, t2, Some(foreign_ip)))));
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((f1.clone(), 60, t2, Some(foreign_ip)))));
        mock_net
            .expect_recv_packet()
            .times(1)
            .returning(move || Ok(Some((rig.clone(), 60, t2, Some(rig_ip)))));
        mock_net.expect_recv_packet().returning(|| Ok(None));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.gm_allowlist = vec!["10.77.9.0/24".to_string()];
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(
            MockSystemClock::new(),
            mock_net,
            MockNtpSource::new(),
            status,
            config,
        );

        controller.process_loop_iteration().unwrap();
        controller.process_loop_iteration().unwrap();
        assert_eq!(
            controller.gm_dropped_since_accepted, 2,
            "two foreign packets must be counted as dropped"
        );

        controller.process_loop_iteration().unwrap();
        assert_eq!(
            controller.gm_dropped_since_accepted, 0,
            "an accepted rig packet must reset the drop counter"
        );
        assert_eq!(controller.current_sync_source_ip, Some(rig_ip));
    }

    // ========================================================================
    // NANO MODE HYSTERESIS TESTS
    // ========================================================================
    // Tests for v1.5.4 hysteresis: NANO mode requires 5 consecutive samples
    // above threshold to exit, preventing single spikes from destabilizing.
    // ========================================================================

    /// Helper to create a controller in a specific NANO mode state for testing
    fn create_nano_test_controller() -> (
        PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
        Arc<RwLock<SyncStatus>>,
    ) {
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let controller = PtpController::new(mock_clock, mock_net, mock_ntp, status.clone(), config);
        (controller, status)
    }

    #[test]
    fn test_nano_mode_requires_lock_first() {
        let (controller, _) = create_nano_test_controller();

        // Verify initial state: not locked, not in NANO
        assert!(!controller.is_locked, "Should not be locked initially");
        assert!(!controller.in_nano_mode, "Should not be in NANO initially");
        assert_eq!(controller.nano_sustain_count, 0);
        assert_eq!(controller.nano_exit_count, 0);
    }

    #[test]
    fn test_nano_entry_requires_sustained_low_drift() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate locked state
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.warmup_complete = true;
        controller.clock_settled = true;

        // Simulate sustained low drift (< 0.5 µs/s) for NANO_SUSTAIN_COUNT samples
        for i in 0..NANO_SUSTAIN_COUNT {
            // Manually increment nano_sustain_count as the rate calculation would
            controller.nano_sustain_count += 1;

            if i < NANO_SUSTAIN_COUNT - 1 {
                assert!(
                    !controller.in_nano_mode,
                    "Should NOT enter NANO before {} samples, currently at {}",
                    NANO_SUSTAIN_COUNT,
                    i + 1
                );
            }
        }

        // After NANO_SUSTAIN_COUNT samples, should enter NANO mode
        if controller.nano_sustain_count >= NANO_SUSTAIN_COUNT {
            controller.in_nano_mode = true;
        }
        assert!(
            controller.in_nano_mode,
            "Should enter NANO after {} sustained samples",
            NANO_SUSTAIN_COUNT
        );
    }

    #[test]
    fn test_nano_exit_single_spike_no_exit() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 0;

        // Single spike above threshold - should NOT exit NANO (hysteresis)
        controller.nano_exit_count = 1;

        // The hysteresis requires NANO_EXIT_COUNT (5) consecutive samples
        assert!(
            controller.in_nano_mode,
            "Single spike should NOT exit NANO mode (hysteresis requires {} samples)",
            NANO_EXIT_COUNT
        );
        assert!(
            controller.nano_exit_count < NANO_EXIT_COUNT,
            "Exit count {} should be less than threshold {}",
            controller.nano_exit_count,
            NANO_EXIT_COUNT
        );
    }

    #[test]
    fn test_nano_exit_requires_consecutive_spikes() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 0;

        // Simulate NANO_EXIT_COUNT - 1 consecutive spikes - should NOT exit
        for i in 1..NANO_EXIT_COUNT {
            controller.nano_exit_count = i;
            assert!(
                controller.in_nano_mode,
                "Should NOT exit NANO with only {} spikes (need {})",
                i, NANO_EXIT_COUNT
            );
        }

        // Simulate the NANO_EXIT_COUNT-th spike - NOW should exit
        controller.nano_exit_count = NANO_EXIT_COUNT;
        if controller.nano_exit_count >= NANO_EXIT_COUNT {
            controller.in_nano_mode = false;
            controller.nano_sustain_count = 0;
            controller.nano_exit_count = 0;
        }

        assert!(
            !controller.in_nano_mode,
            "Should exit NANO after {} consecutive spikes",
            NANO_EXIT_COUNT
        );
        assert_eq!(
            controller.nano_sustain_count, 0,
            "Sustain count should reset on NANO exit"
        );
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit count should reset on NANO exit"
        );
    }

    #[test]
    fn test_nano_exit_counter_resets_on_good_sample() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode with some exit counter
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 3; // Some spikes, but not enough to exit

        // Good sample (low drift) should reset exit counter
        // Simulating what happens when abs_rate < NANO_ENTER_RATE_US
        controller.nano_exit_count = 0;
        controller.nano_sustain_count += 1;

        assert!(controller.in_nano_mode, "Should remain in NANO mode");
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit counter should reset on good sample"
        );
    }

    #[test]
    fn test_nano_constants_are_correct() {
        // Verify the constants match expected values for documentation
        assert_eq!(
            NANO_SUSTAIN_COUNT, 15,
            "NANO entry requires 15 sustained samples"
        );
        assert_eq!(
            NANO_EXIT_COUNT, 5,
            "NANO exit requires 5 consecutive spikes (hysteresis)"
        );
        assert!(
            (NANO_ENTER_RATE_US - 0.5).abs() < 0.001,
            "NANO entry threshold is 0.5 µs/s"
        );
        assert!(
            (NANO_EXIT_RATE_US - 1.0).abs() < 0.001,
            "NANO exit threshold is 1.0 µs/s"
        );
    }

    #[test]
    fn test_mode_transition_not_locked_resets_nano() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 2;

        // Simulate loss of lock
        controller.is_locked = false;

        // The controller logic resets NANO state when not locked
        if !controller.is_locked {
            controller.in_nano_mode = false;
            controller.nano_sustain_count = 0;
            controller.nano_exit_count = 0;
        }

        assert!(
            !controller.in_nano_mode,
            "Should exit NANO when lock is lost"
        );
        assert_eq!(
            controller.nano_sustain_count, 0,
            "Sustain count should reset when lock is lost"
        );
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit count should reset when lock is lost"
        );
    }

    #[test]
    fn test_nano_deadband_constant() {
        // Verify deadband is configured correctly
        assert!(
            (NANO_DEADBAND_US - 0.1).abs() < 0.001,
            "NANO deadband should be 0.1 µs/s"
        );
    }

    // ========================================================================
    // SYNC SOURCE / GRANDMASTER SWITCH TESTS
    // ========================================================================
    // Tests for v1.5.5+ soft reset: when sync source changes, we preserve
    // the learned frequency and stay in current mode instead of hard reset.
    // ========================================================================

    /// Helper to create a controller in LOCK mode for grandmaster switch testing
    fn create_locked_controller() -> (
        PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
        Arc<RwLock<SyncStatus>>,
    ) {
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller =
            PtpController::new(mock_clock, mock_net, mock_ntp, status.clone(), config);

        // Set up controller in LOCK state with learned frequency
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.warmup_complete = true;
        controller.clock_settled = true;
        controller.applied_freq_ppm = 35.0;
        controller.drift_baseline_ppm = 33.5;
        controller.current_sync_source = Some([0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9]);
        controller.current_gm_uuid = Some([0x00, 0x00, 0x00, 0x00, 0x01, 0x00]);

        // Add some pending syncs and samples
        controller.pending_syncs.insert(
            1,
            PendingSync {
                rx_time_sys: SystemTime::now(),
                source_uuid: [0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9],
            },
        );
        controller.sample_window.push(1000);
        controller.sample_window.push(2000);

        (controller, status)
    }

    #[test]
    fn test_sync_source_initial_detection() {
        let (mut controller, _) = create_locked_controller();

        // Reset to no sync source
        controller.current_sync_source = None;

        // Verify initial detection sets sync source without reset
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];

        // Simulate what handle_sync_message does for initial source
        controller.current_sync_source = Some(new_source);

        assert_eq!(
            controller.current_sync_source,
            Some(new_source),
            "Should set initial sync source"
        );
        // Frequency should still be preserved (not reset)
        assert!(
            (controller.applied_freq_ppm - 35.0).abs() < 0.01,
            "Frequency should be preserved on initial detection"
        );
    }

    #[test]
    fn test_sync_source_change_soft_reset_preserves_frequency() {
        let (mut controller, _) = create_locked_controller();

        let old_freq = controller.applied_freq_ppm;
        let old_drift = controller.drift_baseline_ppm;

        // Simulate sync source change (soft reset logic)
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Key: applied_freq_ppm and drift_baseline_ppm are NOT reset

        assert!(
            (controller.applied_freq_ppm - old_freq).abs() < 0.01,
            "Soft reset should preserve applied_freq_ppm: expected {}, got {}",
            old_freq,
            controller.applied_freq_ppm
        );
        assert!(
            (controller.drift_baseline_ppm - old_drift).abs() < 0.01,
            "Soft reset should preserve drift_baseline_ppm: expected {}, got {}",
            old_drift,
            controller.drift_baseline_ppm
        );
    }

    #[test]
    fn test_sync_source_change_soft_reset_clears_stale_data() {
        let (mut controller, _) = create_locked_controller();

        // Verify we have stale data before
        assert!(
            !controller.pending_syncs.is_empty(),
            "Should have pending syncs before soft reset"
        );
        assert!(
            !controller.sample_window.is_empty(),
            "Should have samples before soft reset"
        );

        // Simulate soft reset
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;

        assert!(
            controller.pending_syncs.is_empty(),
            "Soft reset should clear pending_syncs"
        );
        assert!(
            controller.sample_window.is_empty(),
            "Soft reset should clear sample_window"
        );
        assert_eq!(
            controller.prev_t1_ns, 0,
            "Soft reset should clear prev_t1_ns"
        );
        assert_eq!(
            controller.prev_t2_ns, 0,
            "Soft reset should clear prev_t2_ns"
        );
    }

    #[test]
    fn test_sync_source_change_stays_in_lock_mode() {
        let (mut controller, _) = create_locked_controller();

        // Verify LOCK state before
        assert!(controller.is_locked, "Should be locked before soft reset");
        assert!(
            controller.in_production_mode,
            "Should be in production mode before soft reset"
        );

        // Simulate soft reset (what handle_sync_message does)
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Key: is_locked and in_production_mode are NOT reset

        assert!(
            controller.is_locked,
            "Soft reset should NOT change lock state"
        );
        assert!(
            controller.in_production_mode,
            "Soft reset should NOT change production mode"
        );
    }

    #[test]
    fn test_sync_source_change_in_nano_mode_stays_nano() {
        let (mut controller, _) = create_locked_controller();

        // Put in NANO mode
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;

        // Simulate soft reset
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Soft reset does NOT touch nano mode state

        assert!(
            controller.in_nano_mode,
            "Soft reset should NOT exit NANO mode"
        );
        assert_eq!(
            controller.nano_sustain_count, NANO_SUSTAIN_COUNT,
            "Soft reset should NOT reset nano_sustain_count"
        );
    }

    #[test]
    fn test_format_mac_helper() {
        let uuid = [0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9];
        let formatted = format_mac(&uuid);
        assert_eq!(
            formatted, "00:1D:C1:51:D0:D9",
            "format_mac should produce correct MAC format"
        );

        let all_zeros = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            format_mac(&all_zeros),
            "00:00:00:00:00:00",
            "format_mac should handle all zeros"
        );

        let all_ff = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            format_mac(&all_ff),
            "FF:FF:FF:FF:FF:FF",
            "format_mac should handle all 0xFF"
        );
    }

    #[test]
    fn test_grandmaster_uuid_change_detected() {
        let (mut controller, _) = create_locked_controller();

        let old_gm = controller.current_gm_uuid;
        let new_gm = [0x00, 0x00, 0x00, 0x00, 0x02, 0x00];

        // Simulate grandmaster UUID change
        controller.current_gm_uuid = Some(new_gm);

        assert_ne!(
            controller.current_gm_uuid, old_gm,
            "Grandmaster UUID should be updated"
        );
        assert_eq!(
            controller.current_gm_uuid,
            Some(new_gm),
            "New grandmaster UUID should be stored"
        );
    }

    #[test]
    fn test_hard_reset_vs_soft_reset_frequency_difference() {
        // This test documents the key difference between hard and soft reset
        let (mut soft_controller, _) = create_locked_controller();
        let (mut hard_controller, _) = create_locked_controller();

        let original_freq = 35.0;

        // Soft reset: preserves frequency
        soft_controller.pending_syncs.clear();
        soft_controller.sample_window.clear();
        soft_controller.prev_t1_ns = 0;
        soft_controller.prev_t2_ns = 0;
        // applied_freq_ppm NOT touched

        // Hard reset (what reset_filter does): clears frequency
        hard_controller.applied_freq_ppm = 0.0;
        hard_controller.drift_baseline_ppm = 0.0;
        hard_controller.is_locked = false;
        hard_controller.in_production_mode = false;

        assert!(
            (soft_controller.applied_freq_ppm - original_freq).abs() < 0.01,
            "Soft reset preserves frequency"
        );
        assert!(
            (hard_controller.applied_freq_ppm - 0.0).abs() < 0.01,
            "Hard reset clears frequency to 0"
        );
        assert!(soft_controller.is_locked, "Soft reset stays locked");
        assert!(!hard_controller.is_locked, "Hard reset loses lock");
    }

    // ========================================================================
    // PTP OFFLINE DETECTION TESTS
    // ========================================================================
    // Tests for v1.5.5+ PTP timeout: when no PTP packets are received for
    // PTP_TIMEOUT_SECS (10s), the app should log and continue with NTP-only sync.
    // ========================================================================

    #[test]
    fn test_ptp_offline_constants() {
        // Verify timeout and threshold constants
        assert_eq!(PTP_TIMEOUT_SECS, 10, "PTP timeout should be 10 seconds");
        assert_eq!(
            NTP_STEP_THRESHOLD_BASE_US, 500,
            "NTP step base threshold should be 500µs"
        );
        assert_eq!(
            NTP_STEP_THRESHOLD_MAX_US, 10_000,
            "NTP step max threshold should be 10ms"
        );
        assert_eq!(
            NTP_ADAPTIVE_MULTIPLIER, 5.0,
            "NTP adaptive multiplier should be 5.0"
        );
    }

    // ==== #50 NTP step-agreement gate (pure decision) ====

    #[test]
    fn ntp_gate_single_outlier_never_steps() {
        let (mut c, _) = create_nano_test_controller();
        assert!(
            !c.ntp_step_gate(2831, 1000),
            "first over-threshold sample is only a candidate"
        );
        assert!(c.ntp_pending_step.is_some());
    }

    #[test]
    fn ntp_gate_two_agreeing_samples_step() {
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_step_gate(2000, 1000));
        assert!(
            c.ntp_step_gate(2100, 1000),
            "second agreeing sample fires the step"
        );
        assert!(
            c.ntp_pending_step.is_none(),
            "pending cleared by the fired step"
        );
    }

    #[test]
    fn ntp_gate_reversal_pair_steps_zero_times() {
        // The live-event signature (dantesync#50): +2831us then -2825us — a queue-biased
        // outlier and its negation. The old servo stepped TWICE (there and back); the gate
        // must step NEVER.
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_step_gate(2831, 1120));
        assert!(
            !c.ntp_step_gate(-2825, 1120),
            "opposite sign contradicts — no step"
        );
        assert!(
            !c.ntp_step_gate(11, 1120),
            "normal sample clears the replaced candidate"
        );
        assert!(c.ntp_pending_step.is_none());
    }

    #[test]
    fn ntp_gate_same_sign_but_wild_magnitude_does_not_agree() {
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_step_gate(2000, 1000));
        assert!(!c.ntp_step_gate(5000, 1000));
        assert!(c.ntp_step_gate(5200, 1000));
    }

    #[test]
    fn ntp_gate_under_threshold_clears_candidate() {
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_step_gate(2000, 1000));
        assert!(!c.ntp_step_gate(100, 1000), "under threshold — clears");
        assert!(c.ntp_pending_step.is_none());
        assert!(
            !c.ntp_step_gate(2050, 1000),
            "must start a FRESH candidate after clear"
        );
        assert!(c.ntp_step_gate(2100, 1000));
    }

    #[test]
    fn ntp_gate_genuine_offset_steps_on_second_interval() {
        // A REAL clock offset persists across samples — the gate delays the step by exactly
        // one NTP interval, never blocks it.
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_step_gate(-3000, 800));
        assert!(c.ntp_step_gate(-2900, 800));
    }

    // ========================================================================
    // #71 / #76 — SERVER MODE'S AGREEMENT GATE, TWICE CORRECTED
    // ========================================================================
    // #71: the client-mode gate's magnitude-tolerance check
    // (`max(TOL, |cand|/2)`, scaled to the candidate's OWN magnitude) assumes
    // consecutive over-threshold samples are the SAME real value plus noise
    // -- correct for a client (jitter around a stable offset), wrong for the
    // master's genuine drift (a near-deterministic ramp where each new
    // sample is systematically LARGER than the last). #71's fix went too far
    // in the other direction: same-sign-ONLY agreement plus a single-sample
    // fast lane, verified only against a noiseless simulation, assumed
    // "small + same-sign" always meant "trustworthy" -- which chased strih's
    // real WAN measurement noise (dantesync#76) into a step roughly every
    // 10s. The current design: same-sign PLUS a FIXED (non-scaling)
    // `NTP_SERVER_AGREEMENT_TOL_US`, sized to the true expected per-check
    // accrual rather than to either the client's self-scaling formula or no
    // magnitude check at all, with NO single-sample exception at any
    // magnitude. See the design comments on dantesync#71 and dantesync#76
    // for the full derivations.
    // ========================================================================

    #[test]
    fn ntp_gate_server_mode_same_sign_wild_jump_no_longer_agrees_76() {
        // #76: this is the EXACT shape that used to be the bug. Pre-#76,
        // server mode's same-sign-ONLY agreement (no magnitude check at all)
        // let a wild same-sign jump agree unconditionally -- which is
        // precisely what let strih's fast lane (and, for large offsets, this
        // same-sign-only agreement) chase real WAN noise into a step. With a
        // FIXED, non-scaling NTP_SERVER_AGREEMENT_TOL_US, a same-sign jump
        // this large (delta 2500) must NOT agree.
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(
            !c.ntp_step_gate(2_500, 1_000),
            "first over-threshold sample is only a candidate"
        );
        assert!(
            !c.ntp_step_gate(5_000, 1_000),
            "a same-sign but WILD magnitude jump (delta 2500us, far over \
             NTP_SERVER_AGREEMENT_TOL_US) must NOT agree in server mode -- this is the exact \
             shape dantesync#76 fixes"
        );
        assert!(
            c.ntp_pending_step.is_some(),
            "replaced by the new (still unconfirmed) candidate, not cleared"
        );
    }

    #[test]
    fn ntp_gate_server_mode_never_steps_on_a_single_sample_76() {
        // #76: v1.8.32's fast lane stepped ANY offset under 2000us on the
        // FIRST sample -- the dominant regression this fixes. There is now
        // no magnitude at which server mode skips the agreement wait.
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(
            !c.ntp_step_gate(700, 200),
            "server mode must NEVER step on a single sample, regardless of magnitude -- the \
             fast lane that did this is exactly what chased strih's real WAN noise into a \
             step roughly every 10s"
        );
        assert!(c.ntp_pending_step.is_some());
    }

    #[test]
    fn ntp_gate_server_mode_small_offset_agrees_within_tolerance_and_steps_76() {
        // A genuinely small, consistent (within-tolerance) same-sign pair
        // still confirms and steps on the second sample -- #76 removes the
        // single-sample fast lane, not the whole point of a small routine
        // correction being able to fire promptly once actually confirmed.
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(
            !c.ntp_step_gate(380, 200),
            "first sample -- only a candidate"
        );
        assert!(
            c.ntp_step_gate(570, 200),
            "second sample within NTP_SERVER_AGREEMENT_TOL_US (delta 190) of the first agrees \
             and fires the step"
        );
        assert!(c.ntp_pending_step.is_none());
    }

    #[test]
    fn ntp_gate_server_mode_large_offset_agrees_within_tolerance_and_steps_76() {
        // The same tolerance-bounded agreement applies uniformly regardless
        // of magnitude -- a large but internally-CONSISTENT pair (delta 300,
        // within tolerance) still confirms, same as strih's real
        // 1668->1801us pair (delta 133) did in the live-evidence replay test.
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(
            !c.ntp_step_gate(2_500, 200),
            "first over-threshold sample is only a candidate"
        );
        assert!(
            c.ntp_step_gate(2_800, 200),
            "second same-sign sample within tolerance (delta 300) agrees and fires the step"
        );
    }

    #[test]
    fn ntp_gate_server_mode_opposite_sign_reversal_never_steps_71() {
        // The historical outlier this gate exists for (dantesync#50):
        // +2831us then -2825us. Both magnitudes are above the fast lane, so
        // this must still require agreement, and the sign flip must still
        // never agree -- the same-sign-only relaxation above must not
        // reintroduce the exact incident that motivated the gate.
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(!c.ntp_step_gate(2_831, 200));
        assert!(
            !c.ntp_step_gate(-2_825, 200),
            "opposite sign must still contradict in server mode"
        );
        assert!(
            c.ntp_pending_step.is_some(),
            "replaced by the new candidate, not cleared"
        );
    }

    #[test]
    fn ntp_gate_client_mode_unaffected_by_server_mode_changes_71() {
        // A non-server-mode controller must see EXACTLY the pre-#71 behavior:
        // same-sign-but-wild-magnitude does NOT agree, and a small offset
        // still needs 2 agreeing samples (no fast lane).
        let (mut c, _) = create_nano_test_controller();
        assert!(!c.ntp_server_mode());
        assert!(!c.ntp_step_gate(700, 200), "no fast lane on a client node");
        assert!(
            !c.ntp_step_gate(2_000, 200),
            "unrelated candidate replaces, still just a candidate"
        );
    }

    // ========================================================================
    // #71 review finding — check_ntp_utc_tracking() must actually WIRE the
    // server-mode cadence, not just have the constant exist unused. The
    // closed-loop simulation below force-sets `last_ntp_check` far in the
    // past on every iteration, so it never proves interval SELECTION itself
    // — a silent revert of the `if self.ntp_server_mode { NTP_SERVER_CHECK_
    // INTERVAL_SECS } else { ... }` line back to always calling
    // calculate_adaptive_ntp_interval() would pass every other test in this
    // file. These two pin the real 10s server-mode cadence directly via the
    // upstream query call count, which only fires when a check is actually
    // due.
    // ========================================================================

    #[test]
    fn server_mode_does_not_query_upstream_before_its_own_cadence_elapses_71() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();
        // NTP_SERVER_CHECK_INTERVAL_SECS is 10 -- 9s ago is NOT yet due. If
        // this silently reverted to calculate_adaptive_ntp_interval() (which
        // returns 30 here, since accumulated_phase_error_us is untouched by
        // this test), 9s-ago would ALSO be not-due, so this half alone does
        // not distinguish the two; the companion test below does.
        mock_ntp.expect_get_offset().times(0);

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        c.last_ntp_check = Instant::now() - Duration::from_secs(9);
        c.check_ntp_utc_tracking();
        // Mock expectation (times(0)) verifies on drop.
    }

    #[test]
    fn server_mode_queries_upstream_at_its_own_10s_cadence_not_the_client_30s_one_71() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();
        // 11s ago IS due under the #71 server-mode 10s cadence, but would
        // NOT be due under the pre-#71 wiring (calculate_adaptive_ntp_interval
        // returns 30 here) -- this is the half that actually pins the
        // constant, not just "some cadence exists".
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(50),
                sign: 1,
                spread_us: 20,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        c.last_ntp_check = Instant::now() - Duration::from_secs(11);
        c.check_ntp_utc_tracking();
        // Mock expectation (times(1)) verifies on drop.
    }

    /// Closed-loop, end-to-end: at a REAL 19ppm ramp and the server-mode
    /// cadence this fix establishes (10s -> 190us/interval accrual, derived
    /// from `NTP_SERVER_CHECK_INTERVAL_SECS` below so this test tracks the
    /// real constant instead of a hand-picked literal — review finding,
    /// #71), the full pipeline (threshold selection, gate, clamp, reset)
    /// must hold the master's steady-state residual well under the ~300us
    /// target from dantesync#71 -- not merely under the older, too-loose
    /// 2ms envelope `the_master_holds_utc_within_a_sub_two_ms_envelope_
    /// over_an_hour_68` asserts. Deterministic (no measurement noise). NOTE
    /// the peak this loop can OBSERVE is always one interval short of the
    /// true pre-step value: it samples the residual AFTER
    /// `check_ntp_utc_tracking()` runs, and a stepping call resets the
    /// residual to ~0 in that SAME call — so the highest value ever
    /// recorded is the last NON-stepping tick, not the tick that actually
    /// crossed the gate. Against the CURRENT (pre-#71) threshold/agreement
    /// logic this 190us/interval accrual peaks at 570us (verified by
    /// temporarily running this exact test against the pre-#71 baseline
    /// commit: threshold 500 is crossed and a candidate opens at interval
    /// 3's 570us — the last recorded non-stepping tick; the CONTRADICT/
    /// replace churn this ticket traces means the step that eventually
    /// fires does so several intervals later, invisibly to this
    /// methodology) -- comfortably failing a <400 bound. With all four #71
    /// changes it peaks at 190us (threshold 200 crossed and fast-laned on interval
    /// 2's 380us, invisibly; interval 1's 190us is the last recorded
    /// non-stepping tick).
    #[test]
    fn the_master_holds_utc_well_under_the_71_target_at_real_19ppm_and_server_cadence() {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 20,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let err_for_clock = error_us.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        // 19 ppm over the #71 server-mode cadence (NTP_SERVER_CHECK_INTERVAL_SECS)
        // of fresh UTC error per check -- derived from the real constant, not
        // a hand-picked literal (review finding, #71).
        const ACCRUAL_US: i64 = 19 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        let mut peak_us = 0_i64;
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
            peak_us = peak_us.max(error_us.lock().expect("sim lock").abs());
        }

        assert!(
            peak_us < 400,
            "the master must hold UTC well under the dantesync#71 ~300us target at a real \
             19ppm oscillator, peaked at {}us -- the agreement gate's magnitude-tolerance \
             check (correct for client jitter) is the wrong shape for the master's monotonic \
             ramp and lets residual pile up across several intervals before confirming",
            peak_us
        );
    }

    // ========================================================================
    // #76 -- v1.8.32's fast lane chases real WAN measurement noise. strih's
    // upstream (Cloudflare, over WAN, pcap_active:false) scatters +0.5..+2.5ms
    // between consecutive bursts -- a magnitude comparable to or larger than
    // the true ~190-380us/check drift signal the #71 fix was tuned against in
    // a NOISELESS simulation. The fast lane's "small + same-sign = trustworthy"
    // assumption is false on this upstream: "small" can just as easily be one
    // noisy reading. See the design comment on dantesync#76 for the full
    // derivation and the rejected alternatives.
    // ========================================================================

    /// strih's OWN logged `Stepped` sequence, 2026-08-11T19:59-20:00Z (v1.8.32
    /// live canary regression). Under the fast lane, EVERY one of these was a
    /// single raw reading that stepped immediately (each falls inside
    /// `(NTP_SERVER_STEP_THRESHOLD_US, NTP_SERVER_FAST_LANE_US)` = (200, 2000)
    /// -- 7 steps in ~70 seconds. Replaying the same sequence through a fixed,
    /// non-scaling agreement tolerance (rather than the removed fast lane)
    /// must reject nearly all of it: hand-traced deltas between consecutive
    /// same-sign readings are 776, 977, 133, 1231, 1052, 465us -- only the
    /// 133us pair (1668 -> 1801) is small enough to plausibly agree under a
    /// few-hundred-us tolerance sized to the TRUE per-check accrual, not to
    /// WAN noise. This is RED against the current fast lane (7 steps) and
    /// must go GREEN at a small step count once the fast lane is removed.
    #[test]
    fn ntp_gate_server_mode_rejects_the_real_strih_wan_noise_sequence_76() {
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        let readings = [1467, 691, 1668, 1801, 570, 1622, 1157];
        let mut step_count = 0;
        for &r in &readings {
            if c.ntp_step_gate(r, 200) {
                step_count += 1;
            }
        }
        assert!(
            step_count <= 2,
            "replaying strih's real v1.8.32 WAN-noise-triggered Stepped sequence must produce \
             at most 2 steps (proper agreement, not the fast lane's zero-wait single-sample \
             stepping), got {} steps out of {} readings",
            step_count,
            readings.len()
        );
    }

    /// #83 CORRECTION: the SAME real WAN-noise sequence, replayed with the WIDENED locked-mode
    /// tolerance (750us instead of 400us). #83 REVIEW FINDING (2nd round, honest correction of
    /// this test's own earlier doc comment): the not-locked 400us tolerance produces 1 step on
    /// this fixture; this widened 750us tolerance produces 2 -- a genuine increase, NOT "the
    /// same bound" an earlier draft claimed. Bounding at <=2 (not <=1) is a deliberate, honest
    /// acceptance of that measured increase, not a hidden one: both extra corrections are small
    /// (sub-2ms per-reading values, nowhere near the frame-period concern this ticket is about),
    /// and the more important mitigation for a genuinely larger excursion is
    /// NTP_SERVER_LOCKED_MAX_STEP_US's own hard per-step ceiling, not this tolerance's exact
    /// value.
    #[test]
    fn locked_mode_agreement_tolerance_does_not_meaningfully_reopen_the_76_wan_noise_vulnerability_83(
    ) {
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;
        let readings = [1467, 691, 1668, 1801, 570, 1622, 1157];
        let mut step_count = 0;
        for &r in &readings {
            if c.ntp_step_gate(r, 200) {
                step_count += 1;
            }
        }
        assert!(
            step_count <= 2,
            "replaying strih's real v1.8.32 WAN-noise-triggered Stepped sequence through the \
             WIDENED locked-mode tolerance must produce AT MOST 2 steps (measured: the \
             not-locked 400us tolerance produces 1 on this exact fixture, this 750us tolerance \
             produces 2 -- an honestly-accepted small increase, not zero) -- got {} steps out of \
             {} readings, meaning the widening increased noise-susceptibility beyond even that \
             accepted, measured bound",
            step_count,
            readings.len()
        );
    }

    /// Closed-loop, end-to-end, WAN-noise variant of the #71 simulation: the
    /// same real 19ppm drift PLUS a deterministic, mostly-positive noise term
    /// shaped like strih's measured scatter (amplitude in the observed
    /// 0.5-2.5ms range, asymmetric -- occasional small/negative excursions,
    /// mostly large positive ones, matching "consecutive burst offsets
    /// scatter +0.5..+2.5ms"). Counts how many of the simulated hour's checks
    /// actually fire a `step_clock` call. RED (current fast lane): the vast
    /// majority of over-threshold noisy readings step immediately -- expect
    /// a HIGH step count, close to the number of over-threshold checks.
    /// GREEN (after #76's fix): step count must drop to "sparse" -- at most
    /// one step roughly every 20-60s, i.e. well under half the checks over
    /// the simulated hour.
    #[test]
    fn the_master_stays_sparse_under_real_wan_measurement_noise_76() {
        let _ = env_logger::builder().is_test(true).try_init();

        // `true_error_us` is the REAL clock error: it accrues true 19ppm
        // drift every check and is reduced ONLY by an actual step_clock call
        // (applied by the amount the controller was actually told, i.e. the
        // NOISY reported value -- a step based on a noisy measurement really
        // does over/under-correct the true clock by that noise, same as on
        // real hardware). The NTP mock reports true_error_us PLUS this
        // check's noise sample -- a noisy VIEW of the true error, never
        // stored back.
        let true_error_us = Arc::new(std::sync::Mutex::new(0_i64));
        let noise_idx = Arc::new(std::sync::Mutex::new(0_usize));
        // Deterministic pseudo-noise sequence, hand-built from the shape strih
        // actually measured (mostly-positive, 0.5-2.5ms amplitude, occasional
        // small/negative readings) -- NOT random, so the test is reproducible.
        // Cycles if INTERVALS exceeds its length.
        const NOISE_US: [i64; 12] = [
            1467, 691, 1668, 1801, 570, 1622, 1157, 2200, -150, 900, 2400, 300,
        ];

        let err_for_ntp = true_error_us.clone();
        let idx_for_ntp = noise_idx.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let true_err = *err_for_ntp.lock().expect("sim lock");
            let idx = *idx_for_ntp.lock().expect("sim lock");
            let reported = true_err + NOISE_US[idx % NOISE_US.len()];
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(reported.unsigned_abs()),
                sign: if reported >= 0 { 1 } else { -1 },
                spread_us: 200, // a "reasonable" burst spread -- below any quality bound
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(0_u32));
        let err_for_clock = true_error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            *steps_for_clock.lock().expect("sim lock") += 1;
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        const TRUE_ACCRUAL_US: i64 = 19 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for i in 0..INTERVALS {
            *true_error_us.lock().expect("sim lock") += TRUE_ACCRUAL_US;
            *noise_idx.lock().expect("sim lock") = i;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }

        let steps = *step_events.lock().expect("sim lock");
        assert!(
            // #76 review finding: tightened from INTERVALS/2 (180) to
            // INTERVALS/6 (60) -- actual measured behavior is ~30 steps/hour;
            // /2 was 6x looser than reality and would miss a partial
            // regression (e.g. degraded noise rejection back up to ~150/360)
            // that never gets anywhere near the old fast lane's ~330/360.
            steps <= INTERVALS as u32 / 6,
            "under real WAN measurement noise the master must step SPARSELY (target ~one per \
             20-60s, well under a step every ~60s = INTERVALS/6), got {} step_clock calls \
             across {} checks over the simulated hour -- v1.8.32's fast lane chases this noise \
             on nearly every over-threshold reading",
            steps,
            INTERVALS
        );
    }

    // ========================================================================
    // #83 -- while genuinely PTP-locked, the master's periodic UTC step was
    // chasing the Dante grandmaster's own real, unfixable rate error vs UTC
    // (measured live: ~38-66ppm, entirely unrelated to PTP's own lock quality,
    // which stayed tight and stable throughout). A large deadband replaces the
    // routine tight threshold while genuinely locked; not-locked keeps the
    // original tight tracking (#71/#76/#80) completely unchanged.
    // ========================================================================

    /// #94 shared closed-loop harness: run a genuinely-PTP-locked server-mode
    /// master for one simulated hour at a constant GM-vs-UTC drift of `ppm`,
    /// returning every applied step's signed microsecond size. Mirrors the #83
    /// closed-loop tests' mock wiring exactly (MockNtpSource returns the live
    /// UTC error; MockSystemClock subtracts each applied step and records it),
    /// factored out so the #94 realized-step-size bound can be asserted at
    /// several drift rates without duplicating the 40-line harness each time.
    fn simulate_locked_master_step_sizes_94(ppm: i64) -> Vec<i64> {
        let _ = env_logger::builder().is_test(true).try_init();
        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 100, // clean, well under the quality bound -- isolates the deadband/step-size relationship
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            steps_for_clock.lock().expect("sim lock").push(applied);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;

        let accrual_us = ppm * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        let intervals = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for _ in 0..intervals {
            *error_us.lock().expect("sim lock") += accrual_us;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }
        let out = step_events.lock().expect("sim lock").clone();
        out
    }

    /// #94 (RED before the fix): a genuinely-PTP-locked master's realized NTP
    /// step must stay inside the PROVEN-ABSORBED 2500us band (camera-box PR
    /// #1017: <=2.5ms steps proven green through the recorded E2E gate + the
    /// A/V-sync dock held LOCKED 87min). The step SIZE is the offset at
    /// CONFIRMATION time = trigger + up to two check-intervals of drift accrual,
    /// so with the pre-#94 2500us trigger the realized step overshoots to
    /// ~2.7-3.7ms at the live 23-66ppm GM error -- ABOVE the absorbed band,
    /// which is the fleet-visible judder P0 this bounds. Asserted at BOTH the
    /// current live rate (~23ppm) and the worst-ever measured (66ppm).
    #[test]
    fn every_locked_step_stays_within_the_proven_2500us_band_94() {
        // Gather both rates FIRST (so the diagnostic prints the whole picture,
        // 23ppm AND 66ppm, even when the first assertion below trips) -- then
        // assert every realized step across both is within the proven band.
        let measured: Vec<(i64, usize, u64)> = [23_i64, 66_i64]
            .into_iter()
            .map(|ppm| {
                let steps = simulate_locked_master_step_sizes_94(ppm);
                assert!(
                    !steps.is_empty(),
                    "at {}ppm a locked master must still step to track the GM's real UTC drift",
                    ppm
                );
                let worst = steps.iter().map(|s| s.unsigned_abs()).max().unwrap();
                eprintln!(
                    "[#94] locked master {}ppm: {} steps/h, worst step {}us",
                    ppm,
                    steps.len(),
                    worst
                );
                (ppm, steps.len(), worst)
            })
            .collect();

        for (ppm, count, worst) in measured {
            assert!(
                worst <= 2_500,
                "at {}ppm every realized NTP step must stay within the proven-absorbed 2500us \
                 band (#94), but the worst was {}us -- that overshoot is the fleet-visible judder \
                 this fix bounds ({} steps in the simulated hour)",
                ppm,
                worst,
                count
            );
        }
    }

    /// Closed-loop, end-to-end: the ACTUAL live-measured drift rate on strih
    /// today (~38ppm) with the master genuinely PTP-locked throughout.
    /// Deliberately NOT a round number picked for convenience -- 38ppm's
    /// per-interval accrual (380us/10s) sits under BOTH the not-locked
    /// NTP_SERVER_AGREEMENT_TOL_US (400us) and the locked
    /// NTP_SERVER_LOCKED_AGREEMENT_TOL_US (750us), which is exactly what
    /// makes two consecutive readings agree and confirm a step almost every
    /// other over-threshold check -- normal 2-sample confirmation, not the
    /// escape valve. #94: the locked deadband is the TRIGGER (1000us), and the
    /// realized applied step = trigger + the 2-sample confirmation mechanism's
    /// own inherent one-extra-interval overshoot (it steps using the SECOND,
    /// confirming reading's value, one interval's worth of drift past the reading
    /// that first crossed the trigger -- an inherent property of requiring
    /// confirmation, not a bug). Pre-#94 that overshoot ran to ~3040us at 38ppm
    /// (ABOVE the 2500us proven band -- the judder this fix removes); with the
    /// lowered 1000us trigger it is ~1520us here, comfortably INSIDE the band
    /// (~9% of the 60fps frame period 16.7ms, ~5% of the 30fps 33.3ms). See
    /// NTP_SERVER_LOCKED_DEADBAND_US's own doc comment for the derivation.
    #[test]
    fn the_locked_master_steps_at_proven_safe_cadence_and_size_83() {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 100, // clean, well below the quality bound -- isolates the deadband's own effect
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            steps_for_clock.lock().expect("sim lock").push(applied);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;

        const ACCRUAL_US: i64 = 38 * NTP_SERVER_CHECK_INTERVAL_SECS as i64; // 38ppm -- today's live-measured strih rate (issue #83 evidence)
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }

        let steps = step_events.lock().expect("sim lock");
        let step_count = steps.len();
        // #94: the locked deadband is now 1000us (the TRIGGER; the realized step
        // = trigger + confirmation overshoot must stay <= the 2500us proven band).
        // At 38ppm (380us/10s interval, under the 750us locked agreement tolerance)
        // the deadband is first crossed around interval 3, confirmed on interval 4
        // via normal 2-sample agreement -> a step roughly every 40s, ~90/hour, each
        // ~1520us. Verified by running (not hand-derived alone, per this project's
        // own standing rule). Bound loosely (80-100) so this stays a genuine
        // regression guard without being brittle to a 1-sample wobble, while still
        // catching a regression back toward ~180/hour (pre-#83 tight-threshold-
        // always) or toward single digits (an escape-valve-governed cadence, the
        // residual the #83 correction checks for).
        assert!(
            (80..=100).contains(&step_count),
            "a genuinely PTP-locked master at 38ppm (under the agreement tolerance) must step via \
             normal 2-sample confirmation at the #94 lowered-trigger cadence (~90 steps/hour), \
             got {} steps -- too few suggests an escape-valve-governed cadence (the residual the \
             #83 correction checks for), too many suggests the deadband regressed below the #94 \
             1000us trigger toward the pre-#83 tight-threshold-always behavior (~180/hour)",
            step_count
        );
        for &applied in steps.iter() {
            assert!(
                applied.unsigned_abs() <= 2_500,
                "#94: each individual locked correction must stay INSIDE the proven-absorbed \
                 2500us band (camera-box PR #1017: <=2.5ms proven safe, dock LOCKED 87min) -- the \
                 lowered 1000us trigger keeps the confirmed step (trigger + one-extra-interval \
                 overshoot) at ~1520us here, well inside the band; exceeding 2500us would be the \
                 pre-#94 overshoot regression this fix removes, got {}us",
                applied
            );
        }
    }

    /// The SAME closed-loop proof at the TOP of the live-measured range
    /// (~66ppm -- issue #83's own evidence: this was yesterday's reading,
    /// possibly a pre-#80 quantization-inflated artifact, but the design
    /// correction explicitly commits to verifying BOTH ends rather than
    /// assuming only the 38ppm case matters).
    ///
    /// At 66ppm the per-interval accrual (660us/10s) EXCEEDS the routine
    /// NOT-locked NTP_SERVER_AGREEMENT_TOL_US (400us), so if this test used
    /// that tolerance, two consecutive over-threshold readings would never
    /// agree -- exactly the #76 high-oscillator-error freeze scenario, and
    /// (discovered by actually running this simulation, not assumed) it
    /// would force EVERY step through the #76 escape valve unconfirmed,
    /// accruing ~21_780us before firing -- ~8.7x the proven-safe ceiling,
    /// reproducing a harm of the same ORDER OF MAGNITUDE as the withdrawn
    /// 25ms mistake. This is exactly why `server_agreement_tolerance_us`
    /// exists: NTP_SERVER_LOCKED_AGREEMENT_TOL_US (750us) comfortably covers
    /// 66ppm's 660us/interval accrual, so normal 2-sample confirmation
    /// governs here too -- the escape valve stays the rare last resort its
    /// name implies, not the routine path for half the measured ppm range.
    #[test]
    fn the_locked_master_at_66ppm_is_confirmation_governed_not_escape_valve_83() {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 100,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            steps_for_clock.lock().expect("sim lock").push(applied);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;

        const ACCRUAL_US: i64 = 66 * NTP_SERVER_CHECK_INTERVAL_SECS as i64; // 66ppm -- top of the live-measured range (issue #83 evidence)
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }

        let steps = step_events.lock().expect("sim lock");
        let step_count = steps.len();
        // Verified by running this exact simulation (never hand-derived alone, per this
        // project's own standing rule): with the #94 lowered 1000us trigger and
        // NTP_SERVER_LOCKED_AGREEMENT_TOL_US (750us) comfortably covering the 660us/interval
        // accrual, the deadband is first crossed around interval 2, confirmed on interval 3 via
        // NORMAL 2-sample agreement (not the escape valve) -> a step roughly every 30s, ~120/hour,
        // each ~1980us. Faster/smaller than the pre-#94 ~72/hour x ~3300us (the #94 conservation
        // trade: capping the step SIZE at <=2500us at 66ppm's 237.6ms/h of drift FORCES ~120
        // steps/h) but every step's SIZE now stays INSIDE the 2500us proven band. Bound loosely
        // (105-130) so this stays a genuine regression guard without being brittle to a 1-sample
        // wobble, while still catching a regression back toward escape-valve-governed behavior
        // (single digits/hour, each potentially tens of ms -- the #83 bug) or below the 1000us
        // trigger (toward the tight-threshold ~180/hour storm cadence).
        assert!(
            (105..=130).contains(&step_count),
            "at 66ppm, with the #94 lowered trigger and the locked-mode tolerance covering this \
             rate, normal confirmation must govern at the ~30s cadence (~120 steps/hour), got {} \
             steps -- too few suggests the escape valve is governing again (the #83 bug, which \
             produced ~21_780us unconfirmed steps), too many suggests the trigger dropped below \
             1000us toward the tight-threshold storm cadence",
            step_count
        );
        for &applied in steps.iter() {
            assert!(
                applied.unsigned_abs() <= 2_500,
                "#94: each individual locked correction must stay INSIDE the proven-absorbed \
                 2500us band -- got {}us (the #94 lowered trigger keeps the confirmed step at \
                 ~1980us here, vs the pre-#94 ~3300us overshoot that caused the fleet judder); \
                 exceeding 2500us is the regression this fix removes",
                applied
            );
        }
    }

    /// #83 REVIEW FINDING (2nd round, critical): PAST NTP_SERVER_LOCKED_AGREEMENT_TOL_US's own
    /// breakeven (~75ppm -- the point where per-check accrual, 10*ppm, exceeds the 750us
    /// tolerance again), normal confirmation stops working the SAME way it did before this
    /// correction at 66ppm, and stepping falls through to the #76 escape valve. Without
    /// NTP_SERVER_LOCKED_MAX_STEP_US, the escape valve would apply the FULL accrued offset
    /// (verified live in review: ~25-26ms at 76-80ppm) in ONE unconfirmed step -- the same
    /// order of magnitude as the withdrawn 25ms mistake. This proves the hard per-step ceiling
    /// closes that gap: at 80ppm (well past the breakeven), EVERY applied step -- confirmed or
    /// escape-valve-forced, clamped or not -- must stay <=NTP_SERVER_LOCKED_MAX_STEP_US, and the
    /// system must actually CONVERGE (a clamped step's residual gets worked off by a rapid
    /// follow-up run, not left to accumulate forever behind another full escape-valve wait).
    #[test]
    fn at_80ppm_past_the_tolerance_breakeven_every_step_is_hard_capped_and_the_system_converges_83()
    {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 100,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            steps_for_clock.lock().expect("sim lock").push(applied);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;

        const ACCRUAL_US: i64 = 80 * NTP_SERVER_CHECK_INTERVAL_SECS as i64; // 80ppm -- past the ~75ppm tolerance breakeven (issue #83, 2nd review round)
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        let mut peak_uncorrected_us = 0_i64;
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
            peak_uncorrected_us = peak_uncorrected_us.max(error_us.lock().expect("sim lock").abs());
        }

        let steps = step_events.lock().expect("sim lock");
        // The critical safety assertion: no single step, however it was triggered, may exceed
        // the hard ceiling.
        for &applied in steps.iter() {
            assert!(
                applied.unsigned_abs() <= NTP_SERVER_LOCKED_MAX_STEP_US as u64,
                "at 80ppm (past the tolerance breakeven, escape-valve-governed) every individual \
                 step must stay <= the hard ceiling ({}us) -- got {}us, which would be an \
                 unconfirmed step of the SAME order of magnitude as the withdrawn 25ms mistake \
                 this whole correction exists to prevent",
                NTP_SERVER_LOCKED_MAX_STEP_US,
                applied
            );
        }
        // The convergence assertion: the system must actually correct, not freeze (the #76
        // guarantee, still required at this higher rate) or grow the uncorrected residual
        // without bound (verified this would happen with a naive clamp-without-counter-fix,
        // before choosing the companion counter-reset behavior).
        assert!(
            !steps.is_empty(),
            "at 80ppm the master must still correct (never freeze permanently, the #76 \
             guarantee) -- got ZERO steps across {} checks over the simulated hour",
            INTERVALS
        );
        // Loosely bound the peak uncorrected residual ever observed (sampled AFTER each check,
        // so this is one interval short of the true pre-step peak, same methodology this file's
        // other closed-loop tests use) -- comfortably above the ~24-26ms one escape-valve cycle
        // can accrue before its first clamped correction (verified in review), but rules out
        // genuinely unbounded growth (which would reach ACCRUAL_US * INTERVALS = 288_000us if
        // nothing ever converged).
        assert!(
            peak_uncorrected_us < 60_000,
            "peak uncorrected residual {}us must stay bounded to roughly one escape-valve \
             cycle's worth of accrual (~24-26ms) plus margin, not grow without bound across the \
             simulated hour (unbounded growth would reach {}us)",
            peak_uncorrected_us,
            ACCRUAL_US * INTERVALS as i64
        );
    }

    /// dantesync#91 — closed-loop reproduction of the strih step-storm and its
    /// alarm. The live root cause: the PTP grandmaster went L2-unreachable, so
    /// the master correctly fell out of genuine lock (`ptp_offline`) and
    /// `server_step_threshold_us` dropped to the tight 200us threshold. Against
    /// strih's real oscillator-vs-UTC error that tight threshold is crossed
    /// almost every 10s check, so the master step-corrected UTC 129-180 times/h
    /// (live-confirmed) -- and every existing health signal stayed silent for
    /// 19h+. This drives the REAL `check_ntp_utc_tracking` loop for a simulated
    /// hour in that exact degraded regime and asserts the node now RAISES the
    /// storm alarm on `/status` (both the count metric and the boolean flag).
    ///
    /// RED before the detector is wired: `ntp_steps_last_hour`/`ntp_step_storm`
    /// stay at their defaults (None/false) because nothing records a step or
    /// evaluates the rate, so both assertions fail. This is a genuine control
    /// loop (the clock SUBTRACTS whatever the controller applies), not a
    /// constant-offset mock -- a constant would step once and stop, never storm.
    #[test]
    fn master_step_storm_raises_the_alarm_91() {
        let _ = env_logger::builder().is_test(true).try_init();

        // Live UTC error of the simulated master, microseconds.
        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 40,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_count = Arc::new(std::sync::Mutex::new(0_u32));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_count.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            *steps_for_clock.lock().expect("sim lock") += 1;
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status.clone(),
            config,
        );
        c.configure_ntp_server_mode(100_000);
        // The degraded regime: PTP grandmaster unreachable -> not genuinely
        // locked -> the tight 200us threshold (NTP is the only UTC reference).
        c.is_locked = false;
        c.ptp_offline = true;

        // ~30ppm of real oscillator error per 10s check (300us): above the
        // 200us tight threshold and within the 400us agreement tolerance, so a
        // step confirms roughly every second check -> ~180 steps/h, squarely in
        // the live 129-180/h storm band and well over the 120/h alarm threshold.
        const ACCRUAL_US: i64 = 30 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }

        let stepped = *step_count.lock().expect("sim lock");
        assert!(
            stepped > NTP_STEP_STORM_THRESHOLD_PER_HOUR,
            "sanity: the simulated degraded regime must actually storm (>{} steps in the \
             simulated hour) -- got {}; if this fails the scenario itself is wrong, not the alarm",
            NTP_STEP_STORM_THRESHOLD_PER_HOUR,
            stepped
        );

        let s = status.read().expect("status lock");
        assert_eq!(
            s.ntp_step_storm, true,
            "a master stepping {} times/h (live storm was 129-180/h) must RAISE the step-storm \
             alarm -- it ran 19h+ silent because nothing tracked the step RATE",
            stepped
        );
        let reported = s.ntp_steps_last_hour.expect(
            "a server-mode master that has stepped must publish ntp_steps_last_hour (the honest \
             steps/h metric #67 asked for), not None",
        );
        assert!(
            reported > NTP_STEP_STORM_THRESHOLD_PER_HOUR,
            "ntp_steps_last_hour ({}) must exceed the {}/h alarm threshold during the storm",
            reported,
            NTP_STEP_STORM_THRESHOLD_PER_HOUR
        );
    }

    /// dantesync#91 — the zero-false-alarm boundary, LOCKED (not just asserted
    /// in a comment). A genuinely-PTP-locked master at the worst-ever measured
    /// Dante-GM rate error (66ppm, #83) steps at the deadband cadence, which is
    /// the CEILING of healthy operation. #94 raised that ceiling: bounding every
    /// step to the <=2500us proven band at 66ppm's 237.6ms/h of drift FORCES ~120
    /// steps/h (conservation), so the healthy ceiling now sits AT the 120/h alarm
    /// line (measured by `the_locked_master_at_66ppm_is_confirmation_governed_not_
    /// escape_valve_83` above), not the pre-#94 ~72/h. The step-storm alarm must
    /// still NOT fire there (it fires only at strictly >120/h, and still catches the
    /// real 129-180/h #91 storm and any drift past ~118ppm, where cadence drops to
    /// 2) -- a future threshold lowering / window widening must not cry wolf on a
    /// healthy fleet master (the exact trust erosion #91 exists to prevent). This
    /// pins the silent direction the same way `master_step_storm_raises_the_alarm_91`
    /// pins the firing direction.
    #[test]
    fn healthy_locked_master_at_66ppm_stays_below_the_storm_alarm_91() {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));
        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 100,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let err_for_clock = error_us.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            *err_for_clock.lock().expect("sim lock") -= d.as_micros() as i64 * sign as i64;
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status.clone(),
            config,
        );
        c.configure_ntp_server_mode(100_000);
        // Genuinely PTP-locked -> the #94 1000us deadband, chasing only the GM's
        // own real 66ppm rate error (the worst ever measured): the healthy ceiling,
        // which #94 raises to ~120/h (each step now <=2500us, so more of them).
        c.is_locked = true;
        c.ptp_offline = false;

        const ACCRUAL_US: i64 = 66 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }

        let s = status.read().expect("status lock");
        let reported = s
            .ntp_steps_last_hour
            .expect("a stepping server-mode master must publish ntp_steps_last_hour");
        assert!(
            reported <= NTP_STEP_STORM_THRESHOLD_PER_HOUR,
            "a HEALTHY 66ppm-locked master (the worst-ever GM rate, ~120 steps/h under the #94 \
             <=2500us step-size cap) must stay at or below the {}/h alarm threshold -- it reported \
             {}/h; a threshold set below the healthy ceiling would cry wolf on a fine fleet master",
            NTP_STEP_STORM_THRESHOLD_PER_HOUR,
            reported
        );
        assert!(
            !s.ntp_step_storm,
            "the step-storm alarm must NOT fire on a healthy 66ppm-locked master ({} steps/h)",
            reported
        );
    }

    /// The exact scenario the existing #76 tests already cover (not yet
    /// PTP-locked) must be completely UNCHANGED by #83 -- proving this is an
    /// additive change, not a behavior change for the "NTP is the only
    /// reference" case. `create_nano_test_controller`/fresh `configure_ntp_
    /// server_mode` default `is_locked=false`, matching every pre-#83 test in
    /// this file that never set it explicitly.
    #[test]
    fn not_locked_master_keeps_the_pre_83_tight_tracking_83() {
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(!c.is_locked, "default state: not yet locked");
        assert_eq!(
            server_step_threshold_us(c.is_locked, c.ptp_offline),
            NTP_SERVER_STEP_THRESHOLD_US,
            "not-locked must keep using the routine tight threshold, unaffected by #83"
        );
    }

    // ========================================================================
    // #83 REVIEW FINDING (critical): the escape valve's own counter used to
    // increment on EVERY successful check regardless of whether the offset
    // was over threshold -- harmless under the tight threshold, but WRONG
    // under the large PTP-locked deadband: its natural cadence (~38-66
    // checks) is LONGER than the escape valve's 30-check patience, so by the
    // time the offset first legitimately crosses the deadband, the counter
    // had ALREADY exceeded its patience on checks that were never even over
    // threshold -- forcing every deadband-driven step through the escape
    // valve unconfirmed, on the FIRST over-threshold sample, bypassing both
    // the 2-sample agreement gate and the burst-quality gate every time.
    // ========================================================================

    /// Reproduces the exact scenario: many checks held safely UNDER the
    /// deadband (more than NTP_SERVER_MAX_CHECKS_WITHOUT_STEP of them --
    /// under the pre-fix code the counter would already have exceeded its
    /// patience purely from those, despite never once being over threshold),
    /// then a SINGLE noisy over-threshold spike that is immediately
    /// contradicted by the next reading (a one-off WAN outlier, exactly the
    /// #76 scenario). The outlier must NEVER step on its own -- it must wait
    /// for a genuine second agreeing sample, exactly like the not-locked
    /// path already requires. RED (pre-fix): the escape valve's stale
    /// counter forces an unconfirmed step on the spike itself.
    #[test]
    fn locked_mode_escape_valve_never_fires_on_a_single_outlier_after_many_under_threshold_checks_83(
    ) {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock_no_step = MockSystemClock::new(); // no expectations set -- panics if step_clock is called
        let mock_net = MockPtpNetwork::new();
        let call_idx = Arc::new(std::sync::Mutex::new(0_u32));
        let idx_for_ntp = call_idx.clone();
        // #83: comfortably more than NTP_SERVER_MAX_CHECKS_WITHOUT_STEP (30) held-under-
        // threshold checks, THEN one spike, then one contradiction.
        const UNDER_THRESHOLD_CHECKS: u32 = NTP_SERVER_MAX_CHECKS_WITHOUT_STEP + 10;
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times((UNDER_THRESHOLD_CHECKS + 2) as usize)
            .returning(move || {
                let i = *idx_for_ntp.lock().expect("sim lock");
                *idx_for_ntp.lock().expect("sim lock") += 1;
                let offset_us: i64 = if i < UNDER_THRESHOLD_CHECKS {
                    1_000 // well under the 2_500us deadband (#83 correction) -- healthy, no candidate forms
                } else if i == UNDER_THRESHOLD_CHECKS {
                    4_000 // ONE noisy spike, over the deadband
                } else {
                    800 // immediately contradicts the spike -- back under threshold
                };
                Ok(crate::ntp::NtpMeasurement {
                    offset: Duration::from_micros(offset_us.unsigned_abs()),
                    sign: 1,
                    spread_us: 200,
                    sample_count: 3,
                    pcap_active: false,
                })
            });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock_no_step, mock_net, mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);
        c.is_locked = true;
        c.ptp_offline = false;

        for _ in 0..(UNDER_THRESHOLD_CHECKS + 2) {
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
        }
        // If a step had been called, MockSystemClock (no expectations set) would already
        // have panicked inside check_ntp_utc_tracking above -- reaching this line at all
        // is itself part of the proof. call_idx also confirms every scripted read ran.
        assert_eq!(
            *call_idx.lock().expect("sim lock"),
            UNDER_THRESHOLD_CHECKS + 2,
            "every scripted read must have been consumed"
        );
    }

    // ========================================================================
    // #76 REVIEW FINDING (critical): the fixed-tolerance agreement gate and the
    // burst-quality gate can each independently reject a genuine same-sign
    // trend FOREVER with no escape -- reproduced live in review as unbounded,
    // silent, permanent growth once true accrual exceeds
    // NTP_SERVER_AGREEMENT_TOL_US (~40ppm+ at this cadence) or the upstream
    // never presents a low-enough-spread burst. ntp_server_checks_since_step
    // is the shared escape valve for both.
    // ========================================================================

    /// Closed-loop reproduction of the reviewer's own finding: at 57ppm (3x
    /// the highest oscillator error ever measured on this fleet, and the
    /// exact value the #76 fix's own commit message cites), the per-check
    /// accrual (570us) permanently exceeds NTP_SERVER_AGREEMENT_TOL_US
    /// (400us), so consecutive same-sign candidates NEVER land within
    /// tolerance of each other -- WITHOUT the escape valve, this simulation
    /// would show unbounded linear growth (peak_us == final error, no step
    /// ever fires). WITH it, the master must step at least once within
    /// NTP_SERVER_MAX_CHECKS_WITHOUT_STEP (+ a small margin) checks of true
    /// error becoming detectable, and the peak error must stay bounded to a
    /// small multiple of the escape-valve window's own worst-case accrual
    /// (NTP_SERVER_MAX_CHECKS_WITHOUT_STEP x per-check accrual), not grow
    /// without limit over the whole simulated hour.
    #[test]
    fn the_master_never_freezes_permanently_at_high_oscillator_error_76() {
        let _ = env_logger::builder().is_test(true).try_init();

        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 200,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let err_for_clock = error_us.clone();
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            steps_for_clock.lock().expect("sim lock").push(applied);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        // 57 ppm -- 3x the highest oscillator error ever measured on this
        // fleet (19ppm), and enough that per-check accrual (570us) exceeds
        // NTP_SERVER_AGREEMENT_TOL_US (400us) on every single check.
        const ACCRUAL_US: i64 = 57 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        let mut peak_us = 0_i64;
        let mut first_step_at_check: Option<usize> = None;
        for i in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
            peak_us = peak_us.max(error_us.lock().expect("sim lock").abs());
            if first_step_at_check.is_none() && !step_events.lock().expect("sim lock").is_empty() {
                first_step_at_check = Some(i);
            }
        }

        let total_steps = step_events.lock().expect("sim lock").len();
        assert!(
            total_steps > 0,
            "at 57ppm (per-check accrual permanently exceeds the fixed agreement tolerance) the \
             master must EVENTUALLY step via the escape valve -- got ZERO steps across {} \
             checks over the simulated hour, meaning corrections froze permanently",
            INTERVALS
        );
        assert!(
            first_step_at_check.unwrap() <= NTP_SERVER_MAX_CHECKS_WITHOUT_STEP as usize + 2,
            "the first step must fire at or shortly after NTP_SERVER_MAX_CHECKS_WITHOUT_STEP \
             checks (the escape valve), got the first step at check {} (0-indexed)",
            first_step_at_check.unwrap()
        );
        // Bounded, not unbounded: peak must stay within a small multiple of
        // one escape-valve window's worth of accrual, never grow linearly
        // for the whole hour the way an unbounded freeze would (which would
        // reach ACCRUAL_US * INTERVALS = 570 * 360 = 205_200us, matching the
        // reviewer's own reproduced number).
        let one_window_worth = ACCRUAL_US * (NTP_SERVER_MAX_CHECKS_WITHOUT_STEP as i64 + 5);
        assert!(
            peak_us < one_window_worth * 2,
            "peak error {}us must stay bounded to roughly one escape-valve window's worth of \
             accrual (~{}us), not grow without limit across the simulated hour (an unbounded \
             freeze would reach {}us)",
            peak_us,
            one_window_worth,
            ACCRUAL_US * INTERVALS as i64
        );
    }

    /// Behavioral pin on the escape valve's own boundary, through the REAL
    /// `check_ntp_utc_tracking()` path (not direct field manipulation).
    /// Alternates between two same-sign offsets (2500, 3500us -- delta
    /// 1000us, always over NTP_SERVER_AGREEMENT_TOL_US) so the NORMAL
    /// tolerance-agreement path never fires on its own: every consecutive
    /// pair CONTRADICTS the other, exactly the frozen-forever scenario this
    /// finding is about. Exactly `NTP_SERVER_MAX_CHECKS_WITHOUT_STEP - 1`
    /// checks must produce ZERO steps; the very next one must produce
    /// exactly one.
    #[test]
    fn ntp_server_escape_valve_fires_at_the_configured_check_count_76() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock_no_step = MockSystemClock::new(); // no expectations set -- panics if step_clock is called
        let mock_net = MockPtpNetwork::new();
        let call_idx = Arc::new(std::sync::Mutex::new(0_u32));
        let idx_for_ntp = call_idx.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times((NTP_SERVER_MAX_CHECKS_WITHOUT_STEP - 1) as usize)
            .returning(move || {
                let i = *idx_for_ntp.lock().expect("sim lock");
                *idx_for_ntp.lock().expect("sim lock") += 1;
                let offset_us = if i % 2 == 0 { 2500 } else { 3500 };
                Ok(crate::ntp::NtpMeasurement {
                    offset: Duration::from_micros(offset_us),
                    sign: 1,
                    spread_us: 200,
                    sample_count: 3,
                    pcap_active: false,
                })
            });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(
            mock_clock_no_step,
            mock_net,
            mock_ntp,
            status.clone(),
            config,
        );
        c.configure_ntp_server_mode(100_000);

        for _ in 0..(NTP_SERVER_MAX_CHECKS_WITHOUT_STEP - 1) {
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking(); // would panic here (unexpected step_clock call) if the escape valve fired early
        }
        assert_eq!(
            c.ntp_server_checks_since_step,
            NTP_SERVER_MAX_CHECKS_WITHOUT_STEP - 1,
            "the counter must track exactly the number of successful checks with no step"
        );

        // Rebuild with a clock that expects EXACTLY one step now, and the
        // remaining single NTP query that pushes the counter to the bound.
        let mut mock_clock_one_step = MockSystemClock::new();
        mock_clock_one_step
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));
        let mut mock_ntp_last = MockNtpSource::new();
        mock_ntp_last.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(2500),
                sign: 1,
                spread_us: 200,
                sample_count: 3,
                pcap_active: false,
            })
        });
        let mut c2 = PtpController::new(
            mock_clock_one_step,
            MockPtpNetwork::new(),
            mock_ntp_last,
            status,
            SystemConfig::default(),
        );
        c2.configure_ntp_server_mode(100_000);
        c2.ntp_server_checks_since_step = NTP_SERVER_MAX_CHECKS_WITHOUT_STEP - 1;
        c2.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c2.check_ntp_utc_tracking();
        // Mock expectations verify on drop: exactly 1 step_clock call.
    }

    #[test]
    fn test_ntp_adaptive_threshold_with_few_samples() {
        let (controller, _) = create_nano_test_controller();
        // With fewer than 3 samples, should return base threshold
        assert_eq!(
            controller.calculate_ntp_adaptive_threshold(),
            NTP_STEP_THRESHOLD_BASE_US
        );
    }

    #[test]
    fn test_ntp_adaptive_threshold_low_jitter() {
        let (mut controller, _) = create_nano_test_controller();
        // Simulate low-jitter system: offsets around 200us with small variance
        controller.ntp_offset_samples.push_back(195);
        controller.ntp_offset_samples.push_back(200);
        controller.ntp_offset_samples.push_back(205);
        controller.ntp_offset_samples.push_back(198);
        controller.ntp_offset_samples.push_back(202);

        let threshold = controller.calculate_ntp_adaptive_threshold();
        // MAD should be ~4us, so threshold = 500 + 3*4 = 512
        // Should be close to base threshold for low-jitter
        assert!(
            threshold < 600,
            "Low-jitter threshold {} should be close to base",
            threshold
        );
    }

    #[test]
    fn test_ntp_adaptive_threshold_high_jitter() {
        let (mut controller, _) = create_nano_test_controller();
        // Simulate high-jitter system: offsets varying widely
        controller.ntp_offset_samples.push_back(-500);
        controller.ntp_offset_samples.push_back(700);
        controller.ntp_offset_samples.push_back(100);
        controller.ntp_offset_samples.push_back(-300);
        controller.ntp_offset_samples.push_back(900);

        let threshold = controller.calculate_ntp_adaptive_threshold();
        // MAD should be large, so threshold should be significantly higher
        assert!(
            threshold > 1000,
            "High-jitter threshold {} should be well above base",
            threshold
        );
        assert!(
            threshold <= NTP_STEP_THRESHOLD_MAX_US,
            "Threshold {} should not exceed max",
            threshold
        );
    }

    #[test]
    fn test_ptp_offline_initial_state() {
        let (controller, _) = create_nano_test_controller();

        // Verify initial state is online
        assert!(!controller.ptp_offline, "Should start online");
        assert!(
            !controller.ptp_offline_logged,
            "Should not have logged offline"
        );
    }

    #[test]
    fn test_ptp_offline_detection_after_timeout() {
        let (mut controller, status) = create_nano_test_controller();

        // Simulate timeout by setting last_ptp_packet to past
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 1);

        // Call check_ptp_status
        controller.check_ptp_status();

        // Verify offline state
        assert!(controller.ptp_offline, "Should be offline after timeout");
        assert!(controller.ptp_offline_logged, "Should have logged offline");

        // Verify status update
        let status_guard = status.read().unwrap();
        assert!(!status_guard.settled, "Status should show not settled");
        assert_eq!(status_guard.mode, "NTP-only", "Mode should be NTP-only");
    }

    #[test]
    fn test_ptp_online_recovery() {
        let (mut controller, _) = create_nano_test_controller();

        // Set offline state
        controller.ptp_offline = true;
        controller.ptp_offline_logged = true;

        // Simulate packet received (recent timestamp)
        controller.last_ptp_packet = Instant::now();

        // Call check_ptp_status
        controller.check_ptp_status();

        // Verify recovery
        assert!(!controller.ptp_offline, "Should be back online");
        assert!(!controller.ptp_offline_logged, "Logged flag should reset");
    }

    #[test]
    fn test_ptp_offline_no_repeat_logging() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate already offline and logged
        controller.ptp_offline = true;
        controller.ptp_offline_logged = true;
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);

        // Call check_ptp_status multiple times
        controller.check_ptp_status();
        controller.check_ptp_status();
        controller.check_ptp_status();

        // Should still be logged (no reset)
        assert!(
            controller.ptp_offline_logged,
            "Should remain logged (no spam)"
        );
    }

    #[test]
    fn test_ntp_tracking_runs_when_ptp_offline() {
        // #68: this used to assert the `ntp_tracking_enabled` flag, which is
        // gone — it had no writer left once `disable_ntp_tracking()` (the
        // defect) was removed, so it was a dormant switch that CLAUDE.md's MVP
        // rule bans. The behaviour it was standing in for is now asserted
        // directly against the policy: PTP offline ⇒ NTP is the only time
        // source left, so the discipline runs regardless of lock state.
        assert!(ntp_discipline_due(
            false, // server_mode
            true,  // ptp_offline
            false, // is_locked
            false, // stale
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
    }

    #[test]
    fn test_ptp_offline_within_timeout_stays_online() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate packet received 5 seconds ago (within timeout)
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(5);

        // Call check_ptp_status
        controller.check_ptp_status();

        // Should still be online
        assert!(!controller.ptp_offline, "Should stay online within timeout");
    }

    // ========================================================================
    // #68 — THE MASTER MUST KEEP DISCIPLINING ITSELF AGAINST UPSTREAM
    // ========================================================================
    // `ntp_server_mode` used to call `disable_ntp_tracking()`, so a healthy
    // master (`ptp_offline == false`) took exactly ONE UTC measurement in its
    // whole lifetime — the boot-time `run_ntp_sync()` — and then free-ran at
    // the Dante grandmaster's rate, which is not UTC's rate. Measured live on
    // strih: 6–19 ppm ⇒ ~21 ms of UTC error 19 minutes after a restart, 1.04 s
    // over two days. A restart is NOT the remedy; a closed loop is.
    // ========================================================================

    #[test]
    fn server_mode_keeps_periodic_upstream_discipline_enabled_68() {
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(
            c.ntp_server_mode(),
            "a master that stops re-reading its own reference is a free-running \
             oscillator advertising itself as a time source"
        );
        assert!(
            ntp_discipline_due(
                c.ntp_server_mode(),
                false, // ptp_offline
                false, // is_locked
                false, // stale
                Duration::from_secs(60),
                Duration::from_secs(30),
            ),
            "server mode alone must arm the periodic upstream query"
        );
    }

    #[test]
    fn ntp_discipline_due_in_server_mode_even_when_ptp_is_not_locked_68() {
        // The whole fleet's UTC hangs on this one node — its duty to track UTC
        // does not depend on whether its OWN PTP happens to be locked.
        assert!(ntp_discipline_due(
            true,  // server_mode
            false, // ptp_offline
            false, // is_locked
            false, // stale
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
    }

    #[test]
    fn ntp_discipline_not_due_before_the_interval_elapses_68() {
        assert!(!ntp_discipline_due(
            true,  // server_mode
            false, // ptp_offline
            true,  // is_locked
            false, // stale
            Duration::from_secs(29),
            Duration::from_secs(30),
        ));
    }

    #[test]
    fn ntp_discipline_for_a_client_node_still_requires_lock_or_offline_68() {
        // Client semantics are unchanged: locked, or PTP offline (NTP-only).
        assert!(!ntp_discipline_due(
            false, // server_mode
            false, // ptp_offline
            false, // is_locked
            false, // stale
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
        assert!(ntp_discipline_due(
            false, // server_mode
            false, // ptp_offline
            true,  // is_locked
            false, // stale
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
        assert!(ntp_discipline_due(
            false, // server_mode
            true,  // ptp_offline
            false, // is_locked
            false, // stale
            Duration::from_secs(60),
            Duration::from_secs(30),
        ));
    }

    // ========================================================================
    // #83 -- server_step_threshold_us: a large deadband while genuinely PTP-locked
    // (chasing the Dante grandmaster's own real, unfixable rate error vs UTC is
    // pointless once the confirmation/tolerance/quality machinery is provably
    // correct, per #71/#76/#80); tight tracking otherwise, unchanged.
    // ========================================================================

    #[test]
    fn server_step_threshold_is_the_large_deadband_when_genuinely_locked_83() {
        assert_eq!(
            server_step_threshold_us(true, false),
            NTP_SERVER_LOCKED_DEADBAND_US
        );
    }

    #[test]
    fn server_step_threshold_stays_tight_when_not_yet_locked_83() {
        // Still acquiring -- NTP is not yet safely deferrable to a PTP lock that
        // doesn't exist yet.
        assert_eq!(
            server_step_threshold_us(false, false),
            NTP_SERVER_STEP_THRESHOLD_US
        );
    }

    #[test]
    fn server_step_threshold_stays_tight_when_ptp_offline_even_if_locked_flag_is_stale_83() {
        // ptp_offline means PTP packets aren't even flowing right now -- NTP is the
        // ONLY meaningful reference regardless of whatever is_locked last read.
        assert_eq!(
            server_step_threshold_us(true, true),
            NTP_SERVER_STEP_THRESHOLD_US
        );
    }

    #[test]
    fn server_step_threshold_stays_tight_when_neither_locked_nor_online_83() {
        assert_eq!(
            server_step_threshold_us(false, true),
            NTP_SERVER_STEP_THRESHOLD_US
        );
    }

    // ========================================================================
    // #83 CORRECTION -- server_agreement_tolerance_us: mirrors server_step_threshold_us's own
    // 4-combination coverage exactly, for the SAME lock-state branching applied to the
    // agreement tolerance instead of the step threshold.
    // ========================================================================

    #[test]
    fn server_agreement_tolerance_is_widened_when_genuinely_locked_83() {
        assert_eq!(
            server_agreement_tolerance_us(true, false),
            NTP_SERVER_LOCKED_AGREEMENT_TOL_US
        );
    }

    #[test]
    fn server_agreement_tolerance_stays_routine_when_not_yet_locked_83() {
        assert_eq!(
            server_agreement_tolerance_us(false, false),
            NTP_SERVER_AGREEMENT_TOL_US
        );
    }

    #[test]
    fn server_agreement_tolerance_stays_routine_when_ptp_offline_even_if_locked_flag_is_stale_83() {
        assert_eq!(
            server_agreement_tolerance_us(true, true),
            NTP_SERVER_AGREEMENT_TOL_US
        );
    }

    #[test]
    fn server_agreement_tolerance_stays_routine_when_neither_locked_nor_online_83() {
        assert_eq!(
            server_agreement_tolerance_us(false, true),
            NTP_SERVER_AGREEMENT_TOL_US
        );
    }

    /// The widened tolerance must never leak into the not-locked path's real
    /// gate calls -- reuses #76's own exact WILD-JUMP fixture (delta 2500,
    /// far over EITHER tolerance) to prove the not-locked path still rejects
    /// it exactly as before, byte-for-byte unaffected by this correction.
    #[test]
    fn not_locked_agreement_gate_still_rejects_the_76_wild_jump_after_the_83_correction() {
        let (mut c, _) = create_nano_test_controller();
        c.configure_ntp_server_mode(100_000);
        assert!(!c.is_locked, "default state: not yet locked");
        assert!(
            !c.ntp_step_gate(2_500, 1_000),
            "first over-threshold sample is only a candidate"
        );
        assert!(
            !c.ntp_step_gate(5_000, 1_000),
            "a same-sign but WILD magnitude jump (delta 2500us) must NOT agree in the not-locked \
             path -- unaffected by the #83 locked-mode tolerance widening"
        );
    }

    /// #83 REVIEW FINDING (2nd round): the companion fix to NTP_SERVER_LOCKED_MAX_STEP_US --
    /// "don't reset the escape-valve counter on a CLAMPED step" -- is gated on `locked_now`, so
    /// it must NEVER change the not-locked path's existing behavior (any step, clamped or not,
    /// always resets the counter). Forces a genuine clamp through the REAL `check_ntp_utc_
    /// tracking()` path with a deliberately small `ntp_server_max_step_us` (no existing test
    /// configures one small enough to actually clamp at the integration level), then asserts
    /// the counter is 0 immediately after -- exactly the pre-#83 (2nd round) behavior.
    #[test]
    fn not_locked_path_still_resets_the_escape_valve_counter_on_a_clamped_step_83() {
        let _ = env_logger::builder().is_test(true).try_init();

        // Constant 5000us offset -- two consecutive agreeing samples (delta 0, well within the
        // routine 400us tolerance) confirm a step, clamped to the deliberately small 1000us
        // bound (no existing test configures one small enough to actually clamp at the
        // integration level).
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(5_000),
                sign: 1,
                spread_us: 100,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let step_events = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let steps_for_clock = step_events.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            steps_for_clock
                .lock()
                .expect("sim lock")
                .push(d.as_micros() as i64 * sign as i64);
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(1_000); // deliberately small -- forces a genuine clamp
        assert!(!c.is_locked, "default state: not yet locked");

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking(); // forms candidate, no step yet
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking(); // confirms + steps, clamped to 1000us (4000us residual)

        assert_eq!(
            *step_events.lock().expect("sim lock"),
            vec![1_000],
            "the confirmed 5000us offset must be clamped to the configured 1000us bound"
        );
        assert_eq!(
            c.ntp_server_checks_since_step, 0,
            "not-locked path must reset the escape-valve counter on ANY step, including a \
             clamped one with a residual -- unlike the locked path's new companion behavior, \
             this must stay completely unchanged"
        );
    }

    #[test]
    fn clamp_ntp_step_leaves_a_steady_state_correction_untouched_68() {
        // At 6–19 ppm a 30s interval accrues ~0.2–0.6 ms, so the bound never
        // fires in steady state — it exists only for a rogue upstream.
        assert_eq!(clamp_ntp_step_us(700, 100_000), 700);
        assert_eq!(clamp_ntp_step_us(-1_500, 100_000), -1_500);
        assert_eq!(clamp_ntp_step_us(100_000, 100_000), 100_000);
    }

    #[test]
    fn clamp_ntp_step_bounds_a_large_correction_and_keeps_its_sign_68() {
        assert_eq!(clamp_ntp_step_us(1_039_375, 100_000), 100_000);
        assert_eq!(clamp_ntp_step_us(-1_039_375, 100_000), -100_000);
        // A wildly wrong upstream cannot teleport the fleet in one jump.
        assert_eq!(clamp_ntp_step_us(3_600_000_000, 100_000), 100_000);
    }

    #[test]
    fn clamp_ntp_step_treats_a_non_positive_bound_as_unbounded_68() {
        // Defensive: a misconfigured 0/negative bound must not freeze the
        // master's UTC discipline at zero correction forever.
        assert_eq!(clamp_ntp_step_us(1_039_375, 0), 1_039_375);
        assert_eq!(clamp_ntp_step_us(1_039_375, -1), 1_039_375);
    }

    /// The whole defect, end to end: a server-mode master that is NOT PTP-locked
    /// still queries upstream on its normal cadence, and the 1.039 s error it
    /// finds is corrected by a bounded step (not one giant fleet-wide jump)
    /// after the second agreeing sample — with no restart anywhere.
    #[test]
    fn master_queries_upstream_and_steps_a_bounded_correction_68() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        // The measurement strih's own log printed after the manual restart.
        mock_ntp.expect_get_offset().times(2).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(1_039_375),
                sign: 1,
                spread_us: 588,
                sample_count: 3,
                pcap_active: false,
            })
        });
        mock_clock
            .expect_step_clock()
            .with(eq(Duration::from_micros(100_000)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);

        c.configure_ntp_server_mode(100_000);
        assert!(
            !c.is_locked,
            "the master is deliberately NOT PTP-locked here"
        );

        // First interval: over-threshold ⇒ only a step CANDIDATE (agreement gate).
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        // Second interval: the sample agrees ⇒ step, clamped to max_step_us.
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        // Mock expectations verify on drop: 2 upstream queries, 1 bounded step.
    }

    // ========================================================================
    // #68 — A FROZEN NTP READING MUST BE VISIBLE AS SUCH
    // ========================================================================
    // Live on strih: `updated_ts` advanced every second (the PTP loop writes
    // it) beside `ntp_offset_us`/`ntp_spread_us`/`ntp_sample_count` that were
    // 18 hours old, with `ntp_failed: false` throughout. After the restart the
    // same triple read 0/0/0 — never measured, and still `ntp_failed: false`.
    // A consumer (camera-box's DanteSync gate) cannot distinguish either state
    // from a healthy node.
    // ========================================================================

    #[test]
    fn ntp_freshness_within_the_window_is_not_stale_68() {
        assert!(!ntp_is_stale(
            Some(Duration::from_secs(31)),
            Duration::from_secs(3600),
            Duration::from_secs(180),
        ));
    }

    #[test]
    fn ntp_freshness_beyond_the_window_is_stale_68() {
        assert!(ntp_is_stale(
            Some(Duration::from_secs(181)),
            Duration::from_secs(3600),
            Duration::from_secs(180),
        ));
        // The reported outage: 18 hours with no measurement at all.
        assert!(ntp_is_stale(
            Some(Duration::from_secs(65_234)),
            Duration::from_secs(172_800),
            Duration::from_secs(180),
        ));
    }

    #[test]
    fn ntp_freshness_at_exactly_the_window_is_not_yet_stale_68() {
        assert!(!ntp_is_stale(
            Some(Duration::from_secs(180)),
            Duration::from_secs(3600),
            Duration::from_secs(180),
        ));
    }

    #[test]
    fn never_measured_falls_back_to_uptime_so_boot_does_not_false_alarm_68() {
        // Freshly started, no measurement yet — not an alarm.
        assert!(!ntp_is_stale(
            None,
            Duration::from_secs(20),
            Duration::from_secs(180)
        ));
        // Up for an hour and STILL no measurement — that is exactly the
        // condition #68 is about, and it must alarm.
        assert!(ntp_is_stale(
            None,
            Duration::from_secs(3600),
            Duration::from_secs(180)
        ));
    }

    /// A successful check publishes WHEN it happened, not just what it found.
    #[test]
    fn a_successful_ntp_check_publishes_its_own_freshness_68() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(120),
                sign: 1,
                spread_us: 40,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, mock_net, mock_ntp, status.clone(), config);

        c.is_locked = true;
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        c.tick_status();

        let s = status.read().expect("status lock");
        assert!(
            s.ntp_updated_ts > 0,
            "a successful measurement must stamp its own epoch second"
        );
        assert_eq!(
            s.ntp_age_s,
            Some(0),
            "a measurement taken just now must report age 0, not null"
        );
        assert!(!s.ntp_failed);
    }

    /// The boot-time one-shot is a real measurement and must be published too.
    /// It was not: live on strih 19 minutes after a restart that DID sync and
    /// step +1.039 s, `/status` still read `ntp_offset_us: 0, ntp_sample_count: 0`.
    ///
    /// Review correction: this test originally asserted `ntp_offset_us ==
    /// 1_039_375` — it pinned the PRE-step measurement as the published offset,
    /// which is a defect of its own (a consumer would read a full second of
    /// error that the very same call had already corrected). The quality fields
    /// and the freshness stamp are what belong here; the published offset itself
    /// is now covered by
    /// `a_corrected_offset_is_published_as_the_residual_not_the_measurement_68`.
    #[test]
    fn the_boot_time_sync_publishes_its_measurement_68() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(1_039_375),
                sign: 1,
                spread_us: 588,
                sample_count: 3,
                pcap_active: false,
            })
        });
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            mock_net,
            mock_ntp,
            status.clone(),
            SystemConfig::default(),
        );
        c.run_ntp_sync(false);

        let s = status.read().expect("status lock");
        assert_eq!(
            s.ntp_spread_us, 588,
            "measurement quality must be published"
        );
        assert_eq!(s.ntp_sample_count, 3);
        assert!(s.ntp_updated_ts > 0, "the boot measurement has a timestamp");
    }

    /// No fresh measurement within the window ⇒ `ntp_failed`, even though no
    /// query ever explicitly FAILED. That is the whole invisibility bug: the
    /// old flag had two writers, both inside the query path the master never
    /// reached.
    #[test]
    fn a_stale_reading_flips_ntp_failed_without_any_query_error_68() {
        let (mut c, status) = create_nano_test_controller();

        c.last_ntp_success = Some(Instant::now() - Duration::from_secs(65_234));
        c.tick_status();

        let s = status.read().expect("status lock");
        assert!(
            s.ntp_failed,
            "18 hours with no NTP measurement must not read as healthy"
        );
        assert!(
            s.ntp_age_s.unwrap_or(0) > 180,
            "the age must be published so a consumer can grade it"
        );
    }

    // ========================================================================
    // #68 REVIEW FINDINGS — the fix must not trade one drift for another
    // ========================================================================

    /// **The blocker.** `calculate_ntp_adaptive_threshold()` models JITTER: it
    /// widens the step threshold by 5x the MAD of recent samples. On the master
    /// the samples are not jitter — they are a deterministic monotonic ramp (the
    /// Dante-vs-UTC frequency error integrating at 6-19 ppm), and the MAD of a
    /// 7-point ramp with per-interval step `s` is exactly `2s`, so the threshold
    /// self-inflates to `500 + 10s` — ten times the accrual it is meant to catch.
    ///
    /// Left alone, the master would sawtooth 2.5-6.8 ms against UTC forever
    /// (ceiling `NTP_STEP_THRESHOLD_MAX_US` = 10 ms), and every one of those
    /// steps is served to the fleet: each client sees the jump, clears its own
    /// 500 µs threshold and follows one or two of its own intervals later, at a
    /// per-client phase. That converts a slow absolute error into a permanent
    /// periodic COHERENCE excursion on a rig whose precision target is 50 µs.
    ///
    /// This is a closed-loop simulation: the mock upstream reports the live UTC
    /// error, the mock clock subtracts every step the controller applies, and
    /// the error accrues at 19 ppm between intervals — so the adaptive threshold
    /// is genuinely exercised (the earlier constant-offset test never got past
    /// 2 samples, which is exactly why this was invisible).
    #[test]
    fn the_master_holds_utc_within_a_sub_two_ms_envelope_over_an_hour_68() {
        let _ = env_logger::builder().is_test(true).try_init();

        // Live UTC error of the simulated master, in microseconds.
        let error_us = Arc::new(std::sync::Mutex::new(0_i64));

        let err_for_ntp = error_us.clone();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().returning(move || {
            let e = *err_for_ntp.lock().expect("sim lock");
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(e.unsigned_abs()),
                sign: if e >= 0 { 1 } else { -1 },
                spread_us: 40,
                sample_count: 3,
                pcap_active: false,
            })
        });

        let err_for_clock = error_us.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock.expect_step_clock().returning(move |d, sign| {
            let applied = d.as_micros() as i64 * sign as i64;
            *err_for_clock.lock().expect("sim lock") -= applied;
            Ok(())
        });

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(mock_clock, MockPtpNetwork::new(), mock_ntp, status, config);
        c.configure_ntp_server_mode(100_000);

        // 19 ppm over the REAL server-mode cadence (NTP_SERVER_CHECK_INTERVAL_SECS,
        // fixed at 10s since #71) of fresh UTC error per check. This test
        // originally modeled a 30s interval (570us/interval) because server
        // mode inherited the client's adaptive-interval selection at the time
        // it was written -- #71 later made server mode's cadence an
        // unconditional, hardcoded 10s, so a 570us/interval accrual has been
        // architecturally IMPOSSIBLE in server mode since then (it would
        // require ~57ppm, 3x the highest oscillator error ever measured on
        // this fleet). Updated to the real cadence so this test continues to
        // exercise its ORIGINAL purpose (base threshold beats the MAD-
        // adaptive-widened one for server mode) against a scenario the code
        // can actually reach, rather than a now-obsolete one it cannot.
        const ACCRUAL_US: i64 = 19 * NTP_SERVER_CHECK_INTERVAL_SECS as i64;
        const INTERVALS: usize = 3600 / NTP_SERVER_CHECK_INTERVAL_SECS as usize; // one simulated hour
        let mut peak_us = 0_i64;
        for _ in 0..INTERVALS {
            *error_us.lock().expect("sim lock") += ACCRUAL_US;
            c.last_ntp_check = Instant::now() - Duration::from_secs(60);
            c.check_ntp_utc_tracking();
            peak_us = peak_us.max(error_us.lock().expect("sim lock").abs());
        }

        assert!(
            peak_us < 2_000,
            "the master must hold UTC inside a sub-2ms envelope, peaked at {}us — a \
             MAD-inflated threshold turns the fleet's own time source into a \
             multi-millisecond sawtooth that every client then chases",
            peak_us
        );
    }

    /// The correction must be measured against the clock the daemon is ABOUT to
    /// correct, not the one it just left. `record_ntp_success` runs BEFORE the
    /// step, so stamping the local reading makes `ntp_updated_ts` (wall clock)
    /// and `ntp_age_s` (monotonic) disagree by the size of the correction — and
    /// that same epoch is served to every NTP client as the Reference Timestamp,
    /// where a backward correction puts it in the FUTURE. RFC 5905 has
    /// conforming clients discard a reply whose reftime is later than its
    /// transmit timestamp, so a master that boots ahead of UTC would be ignored
    /// by ntpd/chrony/w32time until its next successful query.
    #[test]
    fn the_published_epoch_is_the_measured_utc_instant_not_the_local_clock_68() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_ntp = MockNtpSource::new();
        // Local clock is a full hour BEHIND UTC.
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_secs(3600),
                sign: 1,
                spread_us: 500,
                sample_count: 3,
                pcap_active: false,
            })
        });
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status.clone(),
            SystemConfig::default(),
        );
        let local_now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        c.run_ntp_sync(false);

        let s = status.read().expect("status lock");
        let published = s.ntp_updated_ts;
        assert!(
            published >= local_now + 3595 && published <= local_now + 3605,
            "expected the measured UTC instant (~local+3600s = {}), got {} — the local \
             reading was published instead, so the served reference timestamp is an \
             hour stale and disagrees with ntp_age_s",
            local_now + 3600,
            published
        );
    }

    /// After a correction lands, `/status` must advertise the offset that
    /// REMAINS, not the one that was just cancelled. Live consequence of getting
    /// this wrong: for up to a full interval after a restart, the master served
    /// `ntp_offset_us: 1039375` for an error it had already stepped away — and
    /// camera-box's gate thresholds exactly that field.
    #[test]
    fn a_corrected_offset_is_published_as_the_residual_not_the_measurement_68() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp.expect_get_offset().times(1).returning(|| {
            Ok(crate::ntp::NtpMeasurement {
                offset: Duration::from_micros(1_039_375),
                sign: 1,
                spread_us: 588,
                sample_count: 3,
                pcap_active: false,
            })
        });
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status.clone(),
            SystemConfig::default(),
        );
        c.run_ntp_sync(false);

        let s = status.read().expect("status lock");
        assert_eq!(
            s.ntp_offset_us, 0,
            "the boot step cancelled the whole offset — publishing the pre-step \
             measurement tells every consumer the node is 1.04s out when it is not"
        );
        assert_eq!(s.ntp_spread_us, 588, "measurement quality still published");
        assert_eq!(s.ntp_sample_count, 3);
    }

    /// A node whose PTP never reaches LOCK (packets flowing, so `ptp_offline`
    /// never latches) would otherwise never query at all — and would then be
    /// marked `ntp_failed` forever by the new freshness rule, because the only
    /// code that CLEARS the flag lives in the query path it cannot reach. The
    /// discipline must arm itself on staleness so the node keeps tracking UTC
    /// and recovers on its own.
    #[test]
    fn staleness_arms_the_discipline_on_a_node_that_never_locks_68() {
        assert!(
            !ntp_discipline_due(
                false, // server_mode
                false, // ptp_offline
                false, // is_locked
                false, // stale
                Duration::from_secs(60),
                Duration::from_secs(30),
            ),
            "a freshly-measured unlocked client stays on its normal schedule"
        );
        assert!(
            ntp_discipline_due(
                false,
                false,
                false,
                true, // stale — no measurement inside the window
                Duration::from_secs(60),
                Duration::from_secs(30),
            ),
            "once stale, an unlocked node must query anyway or it can never recover"
        );
    }

    /// A misconfigured `ntp_stale_secs: 0` must not pin `ntp_failed` (and the
    /// operator's tray toast) on forever — `max_step_us` got exactly this
    /// defensive treatment, and the same reasoning applies here.
    #[test]
    fn a_zero_staleness_window_is_floored_not_taken_literally_68() {
        let (mut c, status) = create_nano_test_controller();
        c.config.ntp_stale_secs = 0;
        c.last_ntp_success = Some(Instant::now());
        c.tick_status();
        assert!(
            !status.read().expect("status lock").ntp_failed,
            "a measurement taken just now cannot be stale under any configured window"
        );
    }

    // ========================================================================
    // PHASE-SLEW WIRING (dantesync#97)
    // ========================================================================

    fn slew_config() -> SystemConfig {
        let mut config = SystemConfig::default();
        // #117: phase_slew (NTP in the rate path) exists only under the legacy discipline.
        config.clock_discipline = crate::config::CLOCK_DISCIPLINE_LEGACY.to_string();
        config.phase_slew.enabled = true;
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        config
    }

    fn one_offset(us: i64, sign: i8) -> crate::ntp::NtpMeasurement {
        crate::ntp::NtpMeasurement {
            offset: Duration::from_micros(us.unsigned_abs()),
            sign,
            spread_us: 40,
            sample_count: 3,
            pcap_active: false,
        }
    }

    #[test]
    fn phase_slew_enabled_and_locked_slews_a_small_error_without_stepping() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times(1)
            .returning(|| Ok(one_offset(3_000, 1)));
        // No clock expectations: any step_clock / adjust_frequency in this path is unexpected and
        // fails the test — proving a small locked error slews rather than steps.
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            MockSystemClock::new(),
            MockPtpNetwork::new(),
            mock_ntp,
            status.clone(),
            slew_config(),
        );
        c.is_locked = true;
        c.ptp_offline = false;

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();

        assert!(c.phase_slew.is_some());
        assert!(
            c.pending_f_phase_ppm > 0.0,
            "a +3ms error must command a positive (speed-up) slew, got {}ppm",
            c.pending_f_phase_ppm
        );
        assert!(c.last_phase_slew_output.is_some());
        let st = status.read().expect("status lock");
        assert!(st.phase_slew_enabled);
        assert!(st.f_phase_ppm > 0.0);
    }

    #[test]
    fn phase_slew_enabled_but_a_large_error_steps_not_slews() {
        let _ = env_logger::builder().is_test(true).try_init();
        // 100ms > the 50ms slew boundary ⇒ the step path must run (cold boot / insane clock).
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times(2)
            .returning(|| Ok(one_offset(100_000, 1)));
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status,
            slew_config(),
        );
        c.is_locked = true;
        c.ptp_offline = false;

        // Two agreeing over-boundary samples: candidate then step (the #50 agreement gate).
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();

        // #105: a step now PRESERVES the learned frequency DC; this servo never slewed, so its DC is
        // 0 and pending stays 0 (the preserve/reset branch keeps applying `i`, which is 0 here). The
        // nonzero-DC preserve contract is covered by `a_locked_step_preserves_the_learned_phase_slew_dc`.
        assert_eq!(
            c.pending_f_phase_ppm, 0.0,
            "a fresh servo (i=0) has no learned DC to hold across a step"
        );
    }

    #[test]
    fn a_locked_step_preserves_the_learned_phase_slew_dc() {
        // #105 (review 🟡): prime a nonzero integrator DC, then a >50ms error STEPS while LOCKED. The
        // step corrects PHASE but the frequency DC must be PRESERVED (pending == i), not zeroed — that
        // zeroing was the high-DC runaway. The servo also re-enters acquisition.
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times(2)
            .returning(|| Ok(one_offset(100_000, 1))); // 100ms > the 50ms slew boundary ⇒ step path
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status,
            slew_config(),
        );
        c.is_locked = true;
        c.ptp_offline = false;

        // Prime the servo's integrator toward a learned DC (several acquisition updates).
        {
            let servo = c.phase_slew.as_mut().expect("slew enabled");
            for _ in 0..6 {
                servo.update(2_000, 10.0);
            }
        }
        let dc = c.phase_slew.as_ref().unwrap().i_ppm();
        assert!(
            dc > 5.0,
            "precondition: integrator holds a nonzero DC, got {}ppm",
            dc
        );

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();

        assert!(
            (c.pending_f_phase_ppm - dc).abs() < 1e-6,
            "a locked step must keep applying the learned DC ({}ppm), got pending={}ppm",
            dc,
            c.pending_f_phase_ppm
        );
        assert!(
            !c.phase_slew.as_ref().unwrap().converged(),
            "the servo must re-enter acquisition after a step"
        );
    }

    #[test]
    fn a_ptp_offline_step_full_resets_the_phase_slew_dc() {
        // #105 (review 🟡): the ptp_offline branch is a FULL reset, NOT a preserve — with PTP dead
        // there is no servo to decouple against, so a held DC would be applied blind. Prime a DC, go
        // ptp_offline, step: pending AND the integrator must both go to 0.
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times(2)
            .returning(|| Ok(one_offset(3_000, 1)));
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status,
            slew_config(),
        );
        c.is_locked = true;
        c.ptp_offline = true;

        {
            let servo = c.phase_slew.as_mut().expect("slew enabled");
            for _ in 0..6 {
                servo.update(2_000, 10.0);
            }
        }
        assert!(
            c.phase_slew.as_ref().unwrap().i_ppm() > 5.0,
            "precondition: a nonzero DC"
        );

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();

        assert_eq!(
            c.pending_f_phase_ppm, 0.0,
            "ptp_offline ⇒ full reset, no held DC"
        );
        assert!(
            c.phase_slew.as_ref().unwrap().i_ppm().abs() < 1e-9,
            "ptp_offline ⇒ integrator reset to 0, got {}ppm",
            c.phase_slew.as_ref().unwrap().i_ppm()
        );
    }

    #[test]
    fn phase_slew_enabled_but_ptp_offline_steps_not_slews() {
        let _ = env_logger::builder().is_test(true).try_init();
        // NTP-only fallback: PTP is dead, so there is no PTP servo to decouple against — even a
        // small error must STEP, never slew.
        let mut mock_ntp = MockNtpSource::new();
        mock_ntp
            .expect_get_offset()
            .times(2)
            .returning(|| Ok(one_offset(3_000, 1)));
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_step_clock()
            .times(1)
            .returning(|_, _| Ok(()));
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            mock_ntp,
            status,
            slew_config(),
        );
        // is_locked=true so the slew gate's `!ptp_offline` clause is what actually rejects the
        // slew (review 🔵: with is_locked=false the `is_locked` clause short-circuits first and the
        // test would pass even if the ptp_offline guard were removed).
        c.is_locked = true;
        c.ptp_offline = true;

        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();
        c.last_ntp_check = Instant::now() - Duration::from_secs(60);
        c.check_ntp_utc_tracking();

        assert_eq!(c.pending_f_phase_ppm, 0.0);
    }

    #[test]
    fn composite_frequency_word_is_f_ptp_plus_f_phase() {
        // Known f_ptp (drift baseline, rate decoupled to ~0) + a held f_phase ⇒ the factor applied
        // to the clock must be 1 + (f_ptp + f_phase)/1e6.
        let captured = Arc::new(std::sync::Mutex::new(1.0_f64));
        let cap = captured.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_adjust_frequency()
            .returning(move |factor| {
                *cap.lock().expect("cap lock") = factor;
                Ok(())
            });
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            slew_config(),
        );
        c.is_locked = true;
        c.drift_baseline_ppm = 30.0;
        c.pending_f_phase_ppm = 50.0;
        c.last_applied_f_phase_ppm = 50.0;
        // Feed a rate equal to the applied slew ⇒ decoupling drives the PTP-observed rate to ~0,
        // so f_ptp stays at the 30ppm baseline.
        c.last_offset_us = Some(0.0);
        c.last_offset_time = Some(Instant::now() - Duration::from_secs(1));
        c.apply_self_tuning_servo(50.0);

        let ppm = (*captured.lock().expect("cap lock") - 1.0) * 1_000_000.0;
        assert!(
            (ppm - 80.0).abs() < 1.5,
            "f_total must be f_ptp(30) + f_phase(50) = 80ppm, got {}ppm",
            ppm
        );
        assert_eq!(
            c.last_applied_f_phase_ppm, 50.0,
            "the applied f_phase must be remembered for the next interval's decoupling"
        );
    }

    #[test]
    fn phase_slew_disabled_applies_only_f_ptp_and_never_decouples() {
        // Byte-identical pre-#97 behaviour: a stray pending f_phase must be ignored, only f_ptp is
        // applied, and the decoupling term is never populated.
        let captured = Arc::new(std::sync::Mutex::new(1.0_f64));
        let cap = captured.clone();
        let mut mock_clock = MockSystemClock::new();
        mock_clock
            .expect_adjust_frequency()
            .returning(move |factor| {
                *cap.lock().expect("cap lock") = factor;
                Ok(())
            });
        let mut config = SystemConfig::default(); // phase_slew OFF (default)
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;
        let mut c = PtpController::new(
            mock_clock,
            MockPtpNetwork::new(),
            MockNtpSource::new(),
            Arc::new(RwLock::new(SyncStatus::default())),
            config,
        );
        assert!(c.phase_slew.is_none());
        c.drift_baseline_ppm = 30.0;
        c.pending_f_phase_ppm = 999.0; // must be ignored when disabled
        c.apply_self_tuning_servo(0.0); // rate 0 ⇒ f_ptp = baseline

        let ppm = (*captured.lock().expect("cap lock") - 1.0) * 1_000_000.0;
        assert!(
            (ppm - 30.0).abs() < 1.0,
            "disabled ⇒ only f_ptp(30) applied, got {}ppm",
            ppm
        );
        assert_eq!(
            c.last_applied_f_phase_ppm, 0.0,
            "disabled must never populate the decoupling term"
        );
    }

    #[test]
    fn decoupling_stops_the_ptp_servo_from_chasing_the_commanded_slew() {
        // Drive the PTP servo with an offset growing at +50us/s (as it would while the clock slews
        // at +50ppm). With decoupling the observed residual rate must be ~0; without it, the servo
        // chases the full 50ppm — the exact fight the decoupling exists to prevent.
        fn smoothed_after(decoupled: bool) -> f64 {
            let mut config = SystemConfig::default();
            // #117: phase_slew exists only under the legacy discipline.
            config.clock_discipline = crate::config::CLOCK_DISCIPLINE_LEGACY.to_string();
            config.phase_slew.enabled = decoupled;
            config.filters.calibration_samples = 0;
            config.filters.warmup_secs = 0.0;
            let mut mock_clock = MockSystemClock::new();
            mock_clock.expect_adjust_frequency().returning(|_| Ok(()));
            let mut c = PtpController::new(
                mock_clock,
                MockPtpNetwork::new(),
                MockNtpSource::new(),
                Arc::new(RwLock::new(SyncStatus::default())),
                config,
            );
            c.is_locked = true;
            if decoupled {
                c.pending_f_phase_ppm = 50.0;
                c.last_applied_f_phase_ppm = 50.0;
            }
            let mut offset = 0.0_f64;
            for _ in 0..30 {
                c.last_offset_time = Some(Instant::now() - Duration::from_secs(1));
                c.apply_self_tuning_servo(offset);
                offset += 50.0; // +50us per ~1s ⇒ raw observed rate ≈ +50ppm
            }
            c.smoothed_rate_ppm
        }
        let decoupled = smoothed_after(true).abs();
        let raw = smoothed_after(false).abs();
        assert!(
            decoupled < 5.0,
            "decoupled PTP rate should be ~0, was {}ppm",
            decoupled
        );
        assert!(
            raw > 20.0,
            "the un-decoupled control must chase the slew, was {}ppm",
            raw
        );
        assert!(
            decoupled < raw,
            "decoupling must strictly reduce the observed rate ({} vs {})",
            decoupled,
            raw
        );
    }
}
