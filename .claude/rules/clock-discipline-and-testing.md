---
paths:
  - "src/controller.rs"
  - "src/ntp.rs"
  - "src/ntp_server.rs"
  - "src/status.rs"
---

# Disciplining a clock here — and how to test one without fooling yourself

Hard-won on #68 (the NTP master free-ran 1.04 s off UTC over two days). Read this before touching
the NTP discipline, the step thresholds, or any test that drives them.

## A mock that returns a CONSTANT cannot test a control loop

The first behavioural test for #68 fed `MockNtpSource` a fixed +1.039 s offset and asserted the
resulting step. It passed, and it was worthless for the property that mattered: with a constant
offset the sample buffer never reaches 3 entries, so `calculate_ntp_adaptive_threshold()` returns
the base value and **the entire adaptive path is never executed**. A multi-millisecond steady-state
sawtooth sat behind that test, invisible, until an adversarial review derived it on paper.

For anything with memory — an adaptive threshold, a MAD/median window, an EMA, the step-agreement
gate, the spike filter — the mock must **close the loop**:

```rust
let error_us = Arc::new(std::sync::Mutex::new(0_i64));      // the simulated world
// upstream reports the LIVE error …
mock_ntp.expect_get_offset().returning(move || { let e = *err_for_ntp.lock().unwrap(); /* … */ });
// … and the clock SUBTRACTS whatever the controller decides to apply
mock_clock.expect_step_clock().returning(move |d, sign| {
    *err_for_clock.lock().unwrap() -= d.as_micros() as i64 * sign as i64; Ok(())
});
```

then drive N intervals, accrue drift between them, and assert an **envelope** (peak error, step
rate) rather than a single call. See
`controller::tests::the_master_holds_utc_within_a_sub_two_ms_envelope_over_an_hour_68` — it runs a
simulated hour at 19 ppm and measures 6270 µs peak before the fix, <2000 µs after.

Time is the other half of this: `check_ntp_utc_tracking()` gates on `last_ntp_check.elapsed()`
against a 10-30 s adaptive interval, so a test must age it by hand (`c.last_ntp_check =
Instant::now() - Duration::from_secs(60)`) before each iteration. Controller tests live in the same
module, so they can reach these private fields — use that instead of sleeping.

## MAD-widened thresholds model JITTER — know whether your samples are jitter

`calculate_ntp_adaptive_threshold()` = `500 µs + 5 × MAD(recent offsets)`, clamped to 10 ms. It
exists to stop a loaded LAN provoking constant stepping.

**It is correct for a CLIENT and wrong for the MASTER**, and the discriminator is what the node
measures against:

- A client measures against the master. Both are frequency-locked to the same Dante grandmaster, so
  their relative rate is ~0 and the samples really are jitter around a stable offset. MAD is the
  right statistic.
- The master measures against an external UTC source it does **not** frequency-track. Its samples
  are a deterministic monotonic ramp at the Dante-vs-UTC error (6-19 ppm measured on strih). The MAD
  of a 7-point ramp with per-interval step `s` is exactly `2s`, so the threshold self-inflates to
  `500 + 10s` — **ten times the accrual it is meant to catch** — and the node sawtooths 2.5-6.8 ms.

So server mode deliberately uses `NTP_STEP_THRESHOLD_BASE_US`. The outlier protection the widening
provides is already supplied by the two-agreeing-samples step gate. Before reusing any
variance-derived threshold on a new signal, ask whether that signal is noise or a ramp.

## In this architecture, "slew" can only mean small frequent steps

PTP owns frequency (`adjust_frequency`) and re-measures phase against the Dante GM every 125 ms. Any
frequency offset injected to correct UTC phase is read back by the PTP servo as drift and cancelled
within seconds — the two loops fight and the casualty is the <50 µs precision target. A UTC
correction is therefore always `step_clock`, and the only lever on its aggressiveness is the
threshold and `max_step_us`. Do not add a second servo.

Stepping itself is safe for PTP: the existing post-step machinery (2 s grace, `sample_window.clear()`,
`spike_filter.clear()`, `prev_t1_ns = 0`) absorbs the transient. But every master step propagates to
the fleet one or two client intervals later, so its SIZE is a fleet-coherence budget, not a private
matter.

## `Instant` vs `SystemTime` — this daemon steps its own wall clock

Anything measuring an INTERVAL (staleness windows, grace periods, `last_ntp_*`) must be `Instant`
(monotonic); a `SystemTime` delta is corrupted by the daemon's own `step_clock`. Reserve
`SystemTime` for values that are genuinely wall-clock: an epoch published in `/status` or served on
the wire.

When you stamp such an epoch during a correction, stamp the **measured UTC instant**
(`local_now + offset_us`), not the local reading — the code runs before the step, so the local
reading is wrong by the whole correction, and that same epoch is served as the NTP Reference
Timestamp, where a node running ahead of UTC would advertise a reftime in the FUTURE (RFC 5905 has
conforming clients discard such a reply outright).

## `/status` is a cross-repo contract — additive only

camera-box's DanteSync gate and `src/bin/tray.rs`'s own duplicate struct both parse this JSON. Add
fields with `#[serde(default)]`, never rename or remove, and pin a
`test_sync_status_pre_68_json_still_deserializes`-style test with a literal old blob.

Two semantics worth preserving:

- **"never measured" must not look like "measured just now".** `ntp_age_s` is `Option<u64>` so it
  serializes as an explicit `null`; a plausible-looking `0` is what made the absent measurement read
  as a healthy node.
- **Publish the residual, not the measurement, once a correction has been applied.** Otherwise
  `/status` advertises an error the same call already cancelled — for a whole interval after a
  restart the master reported 1.04 s it had already stepped away, and the gate thresholds exactly
  that field.

Whenever a field's MEANING widens (as `ntp_failed` did, from "a query errored" to "UTC alignment is
not being maintained"), update its doc comment AND every consumer's user-visible copy in the same
change — the tray was still toasting "NTP server unreachable" for a node that had never tried.

## An "agreement" gate designed for jitter is wrong for a ramp — check what the signal actually IS

Hard-won on issue 71 (the server-mode master's steady-state sawtooth). `ntp_step_gate`'s
two-agreeing-samples confirmation (issue 50) requires same-sign AND magnitude-within-tolerance —
correct for a CLIENT, whose over-threshold samples are noisy readings of a roughly STATIONARY true
value (two readings of the same real error should be close in magnitude). It is actively WRONG for
the SERVER-mode master, whose samples are a deterministic MONOTONIC RAMP: each new sample is
systematically LARGER than the last by roughly one interval's accrual, not merely noisily
different. A magnitude-similarity requirement on a ramp routinely CONTRADICTS a genuine trend
instead of confirming it (the next sample "disagrees" simply because the ramp kept moving),
producing a multi-interval reset-pileup worse than the design intended (hand-traced AND
empirically verified: 3 intervals / 1710us peak instead of the intended 2 / 1140us, at the
original 570us/30s model).

**The fix shape for a ramp-shaped signal: same-sign-only agreement (drop magnitude comparison
entirely), plus a "fast lane" that skips the wait for small, routine corrections** (same-sign
persistence across even ONE additional sample is itself the confirming signal on a ramp — it does
not spontaneously reverse direction). Keep the ORIGINAL magnitude-tolerance-style protection (or in
this case, just the same-sign requirement is enough — see below) for LARGE/anomalous offsets, where
a single bad reading producing a big fleet-wide jump is the real risk. Before reusing ANY
agreement/outlier-rejection mechanism on a new signal (not just the NTP step gate — this applies to
any future control-loop confirmation logic in this codebase), ask the same question the MAD-adaptive
threshold section above already asks: **is this signal noise around a stable value, or a
deterministic trend?** A mechanism tuned for one is often actively counterproductive on the other.

Checking whether dropping a magnitude-tolerance check reopens a HISTORICAL incident: re-derive
what actually caught it. Issue 50's own reversal incident (`+2831us` then `-2825us`) is an
OPPOSITE-SIGN pair — the same-sign check alone still catches it with zero magnitude comparison
needed. Don't assume a compound check's protection is inseparable; trace which SPECIFIC part of it
defeated the SPECIFIC historical case before relaxing the rest.

## Verify a hand-derived "expected number" by RUNNING it, never trust the arithmetic alone

A closed-loop simulation test's exact peak number is easy to get wrong by hand, because the
methodology has a non-obvious wrinkle: it samples the residual AFTER `check_ntp_utc_tracking()`
returns, and a call that ITSELF steps resets the residual to ~0 before the sample is taken — so the
observably-recorded peak is always the LAST NON-STEPPING tick, one interval short of the true
pre-step spike, not the naively-expected "threshold + one interval's accrual" figure. On issue 71,
a hand-traced pre-fix number (950us) was off by ~1.7x from the actual measured value (570us).

**When a doc comment or design writeup claims a specific number from this test family, verify it by
actually running the test against the relevant code version — don't trust the arithmetic.** A cheap
way to check an OLDER/BASELINE code path's behavior without a second git checkout: `git show
<sha>:src/controller.rs > /tmp/baseline.rs`, splice in ONLY the test function you want to run (plus
any new constants it references), swap it into place over the working tree temporarily, `cargo test
--lib <test_name>` (see `## Local Build Policy` for the `# airuleset:build-ok` one-off bypass this
needs), read the ACTUAL panic message's number, then restore the real working-tree file. Faster and
safer than a temporary git worktree for a single-file, single-function check.
