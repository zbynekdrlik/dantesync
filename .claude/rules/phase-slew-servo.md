---
paths:
  - "src/phase_slew.rs"
---

# The phase-slew PI servo (#97) — before you touch the gains or the wiring

`src/phase_slew.rs` is a bounded PI servo that corrects a small (<50ms) UTC phase error by a
frequency SLEW composed with the PTP word (`f_total = f_ptp + f_phase`) instead of a step. It is
**default OFF** (`system.phase_slew.enabled`); the canary rollout flips it per box. The controller
wiring lives in `src/controller.rs` (`slew_phase` / `reset_phase_slew`, the decoupling in
`apply_self_tuning_servo`, the slew-vs-step decision in `check_ntp_utc_tracking`). The
`clock-discipline-and-testing.md` "do not add a second servo" rule is AMENDED by this (see its own
#97 note) — a second loop on this clock is safe ONLY with the feed-forward decoupling below.

## The three things that make it safe — do not break any of them

1. **Feed-forward decoupling (the load-bearing invariant).** `decouple_ptp_rate` subtracts the
   commanded `f_phase` from every PTP rate observation BEFORE the PTP servo consumes it, so the PTP
   servo never reads the slew as grandmaster disagreement and cannot fight it. The sign is `-`
   because `offset = local - master` (a faster local clock GROWS the observed offset). Getting the
   sign backwards DOUBLES the disturbance instead of cancelling it — catastrophic on the clock
   authority. The subtracted value is `last_applied_f_phase_ppm` = the slew actually in effect over
   the just-measured interval (set at the END of `apply_self_tuning_servo`, read at the START of the
   next). Keep that timing.

2. **The deadbeat gain cap.** The raw `k_p = 100ppm/ms` is UNSTABLE at the client's slow NTP cadence
   (`check_ntp_utc_tracking` fires every 10-30 s; the discrete phase loop needs `k_p·dt < 2`, and
   `0.1 · 30 = 3.0` is a DIVERGENT limit cycle — proven by simulation, not theory). `effective_kp`
   caps the per-update gain to `1.0/dt` (deadbeat), keeping the exact design `k_p` at the nominal
   ≤10 s (master) cadence and staying stable at any longer one. **If you change `K_P_PPM_PER_US` or
   `DEADBEAT_MAX_GAIN`, re-verify stability by a closed-loop simulation across dt = 10..30 s** (see
   the `deadbeat_cap_keeps_the_client_30s_cadence_stable...` test), never by eyeballing the gain.

3. **The I-integration deadband** (`I_DEADBAND_US`, 150 µs) freezes the integrator on sub-deadband
   jitter so a client's near-zero-mean measurement noise cannot random-walk the frequency, while the
   master's sustained DC error (≈230 µs at 23 ppm) still engages it. Above the LAN client noise
   floor, below the P-equilibrium — keep it in that window if you retune.

## Verify by RUNNING a simulation, never by arithmetic (this repo's own discipline)

Every gain/cap/rate number here was chosen by a throwaway `python3` closed-loop sim (DC inflow +
measurement noise, at both the 10 s and 30 s cadences) BEFORE any Rust was written — the same
"verify a hand-derived number by running it" rule `clock-discipline-and-testing.md` teaches for the
step path. Because a camera-box session cannot compile/run tests locally (that file's own build
note), the sim is doubly important: it is the ONLY pre-CI check of the servo MATH. Reproduce the
scenario, print an ENVELOPE (peak |e|, steady-state, integrator value), and assert against it — a
mock returning a constant cannot test a control loop.

## Canary / re-tighten (the #97 follow-up)

The rollout is per-box (cams → imag → strih LAST). The proof a box is safe: step census 0/24h, |e|
p99 ≤ 1 ms, and — critically — `f_ptp` variance unchanged pre/post (that is the direct evidence the
two servos are NOT fighting, i.e. the decoupling works). Read the phase-servo telemetry from
`/status` (`phase_slew_enabled`, `f_phase_ppm`, `f_phase_p_ppm`/`f_phase_i_ppm`, `f_ptp_ppm`,
`phase_slew_saturated`) and the `[PHASE-SLEW]` / `[PHASE-SLEW][SATURATED]` journal lines. Any
residual copies/gaps AFTER the step storm is removed is a DIFFERENT (emit-side) bug — file it, never
re-relax a gate.
