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

2. **The damped gain cap (RETUNED by #103 — this is where the #97 design went wrong).** `effective_kp`
   caps the per-update gain to `MAX_LOOP_GAIN/dt`, so the loop gain `k_p_eff·dt ≤ MAX_LOOP_GAIN` at
   ANY cadence. #97 shipped this as a DEADBEAT cap (`DEADBEAT_MAX_GAIN = 1.0`, `k_p = 0.1` →
   `k_p_eff·dt = 1.0`), which the #97 convergence tests "proved" stable — but ONLY against a
   ZERO-delay plant. The real loop has ≈1 sample of transport delay (burst-median NTP measurement +
   composite PTP+phase word + integrator pole); a deadbeat loop with one sample of delay
   (`z²−z+k·dt = 0` at `k·dt = 1`) sits ON the unit circle and RINGS into a sustained ±500 µs limit
   cycle — the #103 prod incident that flaked camera-box's 2 ms clock gate (6-sample spread 3-13 ms).
   #103 dropped `k_p` to `0.02` (loop gain 0.2 at 10 s → overdamped) and renamed/lowered the cap to
   `MAX_LOOP_GAIN = 0.25` (a critical-damping ceiling at every cadence, `z²−z+0.25` = double pole 0.5).
   **If you change `K_P_PPM_PER_US` or `MAX_LOOP_GAIN`, re-verify stability with a closed-loop sim
   that INCLUDES ONE SAMPLE OF TRANSPORT DELAY** (`e_{n+1} = e_n + (inflow − f_phase_{n-1})·dt`) — a
   zero-delay sim will hide the very oscillation #103 fixed. See
   `damped_servo_does_not_ring_on_a_one_sample_delay_plant`.

3. **The full phase deadband (widened by #103).** `PHASE_DEADBAND_US` (200 µs; was `I_DEADBAND_US`,
   150 µs) now freezes the proportional term AND the integrator STATE inside the band — no NEW
   response to the sub-deadband error, so the NTP-path noise floor is never chased. **Do NOT read
   this as "f_phase → 0 in the band":** the integrator's already-absorbed DC frequency KEEPS being
   applied (`target = 0 + i` = the held integrator), and that held frequency is exactly what holds
   the clock on-phase against the Dante-vs-UTC drift — zeroing `f_phase` in-band would let the drift
   repop, which IS the #103 failure. Only the *reaction* to the residual is suppressed. 200 µs sits
   above the measurement noise floor (≈40 µs burst spread + ≈130 µs inter-burst jitter; the healthy
   6-sample spread is 119-135 µs) and below the master's P-equilibrium, so a real sustained DC error
   still pushes `|e|` past it and engages the servo. Above the noise floor, below the P-equilibrium —
   keep it in that window if you retune. (The converged-servo behaviour is locked by the
   `deadband_holds_the_converged_dc_frequency…` test — a fresh-servo test alone cannot catch a
   regression to zeroing `f_phase` in-band.)

4. **The output slew-rate limiter (#103).** `F_PHASE_SLEW_RATE_PPM_PER_S` (1.5 ppm/s) bounds how fast
   the commanded `f_phase` (the DEMAND `P + I`) is allowed to move, so a noisy proportional swing
   cannot lurch the clock's frequency — it bounds clock ACCELERATION. `saturated`/`alarm` key on the
   DEMAND, not the rate-limited output, so the saturation alarm still arms correctly. The reported
   `PhaseSlewOutput.f_phase_ppm` is the rate-limited value actually applied, and it is what the
   decoupling feeds back — keep those consistent if you touch the limiter.

## Verify by RUNNING a simulation, never by arithmetic (this repo's own discipline)

Every gain/cap/rate number here was chosen by a throwaway closed-loop sim (DC inflow + measurement
noise, at both the 10 s and 30 s cadences) BEFORE any Rust was written — the same "verify a
hand-derived number by running it" rule `clock-discipline-and-testing.md` teaches for the step path.
Because this repo is **Tier-0 (no local cargo build/test)**, the sim is doubly important: it is the
ONLY pre-CI check of the servo MATH. Reproduce the scenario, print an ENVELOPE (peak |e|,
steady-state, integrator value, AND the worst rolling 6-sample spread = the camera-box gate metric),
and assert against it — a mock returning a constant cannot test a control loop.

**#103 lesson — the sim MUST include transport delay, or it lies.** The #97 sim used a ZERO-delay
plant and "proved" the deadbeat gains stable; they oscillated in prod. A control-loop sim for THIS
servo is only faithful with ≥1 sample of loop delay (`e_{n+1} = e_n + (inflow − f_phase_{n-1})·dt`).
A zero-delay sim is a false-green for any gain change. Verify the actual Rust servo (not just a
replica) by extracting `src/phase_slew.rs` to a scratch file (stub `log::warn` with a `macro_rules!`)
and running `rustc --edition 2021 --test scratch.rs` — the module only depends on `log`, so this runs
the REAL servo law under Tier-0 with no crate build. Prove RED→GREEN this way before pushing.

## Canary / re-tighten (the #97 follow-up)

The rollout is per-box (cams → imag → strih LAST). The proof a box is safe: step census 0/24h, |e|
p99 ≤ 1 ms, and — critically — `f_ptp` variance unchanged pre/post (that is the direct evidence the
two servos are NOT fighting, i.e. the decoupling works). Read the phase-servo telemetry from
`/status` (`phase_slew_enabled`, `f_phase_ppm`, `f_phase_p_ppm`/`f_phase_i_ppm`, `f_ptp_ppm`,
`phase_slew_saturated`) and the `[PHASE-SLEW]` / `[PHASE-SLEW][SATURATED]` journal lines. Any
residual copies/gaps AFTER the step storm is removed is a DIFFERENT (emit-side) bug — file it, never
re-relax a gate.
