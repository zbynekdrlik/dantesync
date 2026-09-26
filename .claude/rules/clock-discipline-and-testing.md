---
paths:
  - "src/controller.rs"
  - "src/ntp.rs"
  - "src/ntp_server.rs"
  - "src/status.rs"
  - "src/ptp_phase_lock.rs"
  - "src/date_offset.rs"
  - "src/time_server.rs"
  - "src/controller/date_sync.rs"
  - "src/controller/date_sync/tests.rs"
  - "src/date_offset/tests.rs"
  - "src/time_server/tests.rs"
  - "tests/two_clock_bench.rs"
  - "tests/simulation_e2e.rs"
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

> LEGACY discipline only since #117 — see "#117 — the old PTP servo was RATE-ONLY" below.

PTP owns frequency (`adjust_frequency`) and re-measures phase against the Dante GM every 125 ms. Any
frequency offset injected to correct UTC phase is read back by the PTP servo as drift and cancelled
within seconds — the two loops fight and the casualty is the <50 µs precision target. A UTC
correction is therefore always `step_clock`, and the only lever on its aggressiveness is the
threshold and `max_step_us`. Do not add a second servo — **UNLESS you decouple it (see #97 below).**

**AMENDED by #97 (`src/phase_slew.rs`), behind a DEFAULT-OFF flag (`system.phase_slew.enabled`).**
The "two loops fight" claim above is only true for a NAIVE second servo. #97 adds a bounded PI
phase-slew servo that IS safe on the same clock via **feed-forward decoupling**: the commanded
`f_phase` (a bounded frequency slew) is composed into the single frequency word `f_total = f_ptp +
f_phase`, and — the load-bearing part — `f_phase` is SUBTRACTED from every PTP rate observation
(`decouple_ptp_rate`) BEFORE the PTP servo consumes it, so the PTP servo never reads the deliberate
slew as grandmaster disagreement and cannot cancel it. The two servos then coexist (PTP owns the
oscillator-vs-GM rate; phase owns UTC phase), with time-constant separation reinforcing it (PTP in
fractions of a second, the phase integrator in minutes). The sign is derived, not assumed (`offset
= local - master`, so a faster local clock grows the offset ⇒ subtract). With the flag OFF (the
default, and the whole fleet until the canary rollout) the frequency- and step-paths are
byte-for-byte the behaviour this section describes — so everything below still holds as the default
reality; #97 is the deliberate, decoupled exception, not a repeal. Any FUTURE second control loop on
this clock still owes the same proof: show the decoupling that keeps it from fighting PTP, or it is
the naive servo this rule bans.

Stepping itself is safe for PTP: the existing post-step machinery (2 s grace, `sample_window.clear()`,
`spike_filter.clear()`, `prev_t1_ns = 0`) absorbs the transient. But every master step propagates to
the fleet one or two client intervals later, so its SIZE is a fleet-coherence budget, not a private
matter. (#97's slew avoids the step entirely for a sub-50ms error while locked — no propagated step,
no grace transient — but keeps the step path for cold boot / |e|>50ms / acquisition / PTP-offline.)

## #117 — the old PTP servo was RATE-ONLY; the phase lock is the default now

**Finding (25.9.2026, the anchors of the #117 design question).** Until #117 the "PTP servo" in
`apply_self_tuning_servo` controlled only `d(offset)/dt`: a P term on the RATE plus an integrator
into `drift_baseline_ppm`. The offset it differentiated was the mod-1 s display phase
(`calculate_phase_offset`), and `initial_epoch_offset_ns` was **written once and never read**. So
nothing held the absolute offset between the box and the grandmaster, and NANO mode additionally
ignored any rate below `NANO_DEADBAND_US` (0.1 µs/s, i.e. up to 360 µs/h of free drift). Two boxes
"locked to the same rate" therefore random-walked apart, and all cross-box wall agreement was
really held by NTP: first by steps (the #67/#83/#88 step storms and chases), then by #97's
`phase_slew`, which did it by steering the RATE up to ±5-19 ppm away from the Dante tick. That
last part is the contract violation #117 removes (owner: RATE = the Dante PTP tick only, NTP =
date stepping only).

**What replaced it (`system.clock_discipline = "ptp_phase_lock"`, the default):**

- `src/ptp_phase_lock.rs`: ONE PI on `e = (t2 − t1) − D` (the raw offset between the two time
  bases, not the mod-1 s phase and not calibration-corrected). It gives rate AND phase from the same
  PTP measurement. The rate servo still does acquisition; once PTP-locked the PI takes the
  frequency word bumplessly. Its `dt` is measured in grandmaster time (`t1`), so loop scheduling
  and the daemon's own wall steps cannot distort it. Critically damped, ~100 s: Kp 0.02/s,
  Ki = Kp²/4.
- `D` (the fleet date offset, `wall = PTP time + D`) is anchored at the first lock, re-anchored
  from the continuous wall on a grandmaster / sync-source change or a > 1 s time-base jump (a GM
  reboot restarts its uptime under the same UUID), and shifted by exactly every applied step.
- `src/date_offset.rs` (#88): only the NTP master reads UTC. It announces a new `D` when
  |UTC − wall| > 50 ms (2 agreeing readings), ≥ 5 s ahead, in the versioned 31900 extension. Every
  box, the master included, applies it at that instant through `DateFollower`.

**The decoupling statement (this rule's standing requirement for any loop on this clock).** There
is no second loop to decouple: the phase lock is the only frequency law once locked, and no
argument of `PhaseLockCore::on_window` carries NTP. The only NTP-derived quantity is `D`. `D`
changes either by a STEP of the wall of exactly the same size at the same instant (`note_step`,
which leaves `e` untouched) or by an absorb of ≤ 100 µs at join time. So NTP contributes nothing
to the rate. For the frequency LAW this is shown by running, not argued: `tests/two_clock_bench.rs`
runs the same PTP world under two different UTC drifts (+8 / −15 ppm vs the GM) and asserts every
box's frequency command sequence is **bit-identical** between the two runs. A future change that
leaks any NTP term into the law fails that assertion. Scope it honestly: the bench drives the
pure modules with the controller's glue mirrored. The controller additionally drops 2 s of PTP
windows after every step (holding the word). Those holds fall at UTC-dependent times, so the
controller's words are NOT bit-identical across UTC scenarios. The hold carries no NTP value,
though, and the bench's `with_grace` variant shows every envelope still holds.

**Cross-box agreement = per-box receive-latency ASYMMETRY.** Every box holds the raw `t2 − t1`
equal to the same `D`, so each wall sits its own one-way PTP latency (network + software
timestamp) behind the grandmaster line. The bench's < 100 µs uses a 25-58 µs delay spread.
The live Windows-Npcap vs Linux-kernel timestamp-latency spread is NOT measured yet. The
canary must read it: compare the boxes' 31900 wall readings, or the camera-box genlock audit.
There is no per-box latency calibration; add one only if the canary shows the spread matters.

**Lessons from the #117 review (each is a test now):**

- **A published `D` must name its time base.** The extension carries the ANCHOR's grandmaster,
  never "the one I hear now". During a GM change those differ for a window, and a box that
  re-anchored first would otherwise adopt a days-wrong offset. Nothing is published while a
  re-anchor is pending.
- **The UUID is not enough: check the time base.** A grandmaster that REBOOTS keeps its UUID and
  restarts its uptime. `date_offset::same_time_base` compares both nodes' PTP "now"
  (`wall − D`, independent of either wall's error). The bench's negative control, with the check
  removed, shows walls ~11.7 days apart.
- **Offsets that take effect "now" are backdated 1 s** (`IMMEDIATE_BACKDATE_NS`). Otherwise a
  follower whose PTP view trails by µs reads a rebase as a pending step and schedules a µs step.
- **A follower must be able to STOP following.** After 30 s with no applicable reply it forgets
  the authority and returns to the local NTP date path. Otherwise a silent master leaves it
  drifting at the GM-vs-UTC rate while it still reports "follower".
- **Take PTP "now" from the D IN EFFECT, never from the published D (round 2).** The published D
  carries a pending step, and an announced step is larger than the step bound by construction
  with no upper limit. A master whose boot NTP failed announces seconds. So the extension carries
  the replier's own `now_ptp_ns = wall − D in effect`, and `same_time_base` compares that. The
  bench's `3 s first step` scenario was RED with the published D.
- **One box's fault never moves the fleet D (round 2).** While ONLY the master lacks PTP, it
  runs the local NTP path on its own wall and D. The authority keeps publishing the fleet D,
  moved only by coordinated announces (see the round-3 fleet-line feed below). Followers hold the
  fleet line, and none of the master's own steps reaches them. Once its PTP is
  back, the master steps its OWN wall onto the fleet line (`realign_master_to_fleet`: one Join,
  which also removes the phase error the outage left, measured on a window taken after PTP
  returned). A failed step on the master works the same way: the fleet D stays, the master
  retries after a 10 s backoff (it keeps feeding the fleet line's UTC error meanwhile). A
  re-engagement > 1 ms off `D` is `Realigned` (same base, the wall wandered) and is never
  `Rebased` (which is a new time base, and the only event that moves the fleet D without a
  step). An earlier fix moved the fleet D with the master's local steps; the `master-only
  outage` scenario showed it reaching every follower as a late step (walls 821 µs apart).
- **The authority is fed the FLEET line's UTC error, not the master's own (round 3).** It is
  `reading + (anchor − fleet)`, fed even while ONLY the master lacks PTP. Otherwise a long outage
  freezes the fleet D: +8 ppm is 29 ms/h, and the 3 h bench scenario breached the bound (a
  94 ms catch-up). An off-line master announces for the fleet but does not schedule the step
  for its own wall (it re-aligns later). The only error left is its free-run drift, ≪ the bound.
- **A rebase shifts the fleet D by the observed BASE SHIFT** (`fleet_old + (new − old)`), never
  onto the master's own anchor. That anchor may be off the fleet line, and folding its offset in
  reached followers as a step (round 3, RED).
  - **Known double-fault limit:** if the grandmaster changes DURING the master's own PTP outage,
    the observed shift also contains the master's untracked free-run error; it had no PTP to
    measure it. That error is about (held-frequency error) × (outage length): about 100 µs for
    the bench's 30 min outage holding the learned integrator, and ms for a multi-hour one.
    Followers take it at their next poll, as one late step above the 100 µs absorb tolerance or
    as an absorb the phase lock slews out in ~2 min. Only a follower's continuous line knows the
    exact shift.
- **A master's local step drops its own SCHEDULED step** (`cancel_pending`, it re-aligns
  anyway). A FOLLOWER's local step keeps it: its NTP source is the master, whose wall has not
  stepped yet. `DateFollower::forget` (authority loss) also KEEPS a scheduled step, since the
  rest of the fleet applies it.
- **No PTP, no phase lock (rounds 3-4):** on the offline EDGE a box drops every pre-outage
  measurement: both windows, pending syncs, the rate tracker, and the PI's `dt` base. Otherwise a
  pre-outage median hides the free-run (e = 0) and it is slewed for minutes. It also disengages
  the core and APPLIES the learned integrator (not the last word with its P term). The next
  window, which holds only post-outage samples, re-engages and is `Realigned` if the wall
  free-ran > 1 ms.
- **Publish immediately what the 31900 server serves (round 4).** The time server reads the
  status SNAPSHOT at reply time. An off-line master's announce, or its local step, must call
  `update_shared_status` at once. The 10 s `tick_status` comes after the 5 s lead, so waiting
  for it made followers step late. The bench models the snapshot and its publish points so this
  class stays covered.
- **A `"DSYX"` request is padded to 64 bytes; a shorter one is ignored (round 5).** The extended
  reply is 104 bytes. Answering an 8-byte request would make every spoofed one a 13x amplifier;
  a request the size of the base reply keeps the ratio at ~1.6. `"DSYN"` is unchanged (8 bytes,
  64-byte reply, the pre-existing 8x). The cost (round 6): an OLD Windows server reads into an
  8-byte buffer, so the padded datagram fails its `recv_from` with `WSAEMSGSIZE` and it logs a
  socket error per poll. The rollout upgrades the NTP master right after the canary
  (`.claude/skills/dantesync-deployment.md`, step 4).
- **The authority poller backs off only for a host that NEVER answered (rounds 5-6).** After 60
  unanswered polls such a host (a public NTP server, an older master, a firewall) is polled every
  30 s; its first reply restores 1 s for good. A host that answered once is never slowed: after a
  reboot it may announce a step 5 s ahead, and a 30 s poll would hear it after the instant (a
  late step). Pinned: `PollBackoff` tests assert the poll stays ≤ `MIN_STEP_LEAD_NS / 2` once
  answered.
- **Whole-fleet PTP loss (accepted):** every box runs the local NTP path against the master. When
  PTP returns, each re-joins the fleet line at its own poll: an uncoordinated step of at most
  the fleet's UTC error (≤ 50 ms), then coherent again.
- **"late" counts real misses only in normal operation.** It also counts the double fault above
  and a GM-change re-anchor residual > 100 µs. It is a health counter, and over-counting is the
  conservative side.
- The follower still publishes its adaptive `ntp_step_threshold_us`. Its `ntp_offset_us` is now
  a µs-level health signal against the master, and ≥ 500 µs is the right envelope to grade it.
  Only the master's UTC error uses the 50 ms authority bound.

**Consequences for code and tests here:**

- The old "In this architecture, 'slew' can only mean small frequent steps" section above
  describes the LEGACY discipline. Under the phase lock a follower never steps on its own NTP
  reading; the master never steps at NTP time. `phase_slew` exists only under
  `clock_discipline = "legacy"` (the #97/#105 tests pin that explicitly).
- Without an authority (an older master that ignores `"DSYX"`, a grandmaster the master is not
  on, PTP offline) a box keeps the existing NTP step path as its LOCAL date fallback, with a
  pure-PTP rate. That is the canary-safe path. Each local step shifts `D` with the wall.
- A bench/sim clock must keep its steps APART from the oscillator-driven reading
  (`wall = continuous + stepped`). Folding a step into an integer-ns clock with a fractional
  carry perturbs the carry, which makes two runs that step at different times diverge in the last
  bit. That is a false failure of the decoupling assertion (hit while building the bench).
- Test a scheduled step's SIMULTANEITY by its landing instant (resolved inside the window), not
  by sampling at window boundaries. An announce's instant is computed on the master's own window
  grid, so it lands within µs of a boundary by construction, and boundary sampling reports a
  spurious 50 ms "disagreement" for the few µs the step is genuinely in flight.
- In the controller, the step lands within one loop iteration of its instant (1 ms Linux /
  50 µs Windows) plus the cross-box wall disagreement (µs). For that window the fleet genuinely
  differs by the step size. It is the only disagreement a coordinated step leaves.

## #119 — a BACKWARD date correction is a coordinated SLEW, never a step

A backward step runs wall time back on every box at once; the camera-box stream OBS lost 43.7 ms
of Dante audio at a −51 ms fleet step, and nothing at the forward ones (camera-box#1372). So:

- **The direction decision is `date_offset::correction_kind`**: `≥ 0` → coordinated step,
  `< 0` → coordinated slew (`DateSlew`: `from → to` at `ppm` from `start`, a pure function of PTP
  time, `offset_at` floored to the ns and exact at `end`). The authority's slew is its announce
  until complete; a slew in progress judges readings by the error LEFT once it has paid, extends
  (continuous, same rate, re-announced from `D` now) only while ≥ one lead is left, and makes a
  forward need wait for its end.
- **Every box holds the slew in `DateFollower`** (`HeldSlew { slew, ref, carry }`):
  `D = anchor + carry + offset_at(ptp) − ref`. The anchor stays the phase lock's BASE; the paid
  amount is folded into it when the slew completes (`D` unchanged). Joining mid-way lands once on
  the fleet's current `D` (join/absorb/late rules) and slews the rest; any replacement keeps the
  displacement so far as `carry`, so `D` never jumps.
- **The decoupling statement for the slew** (this rule's standing requirement): the slew enters
  the clock ONLY as a rate term of the one frequency word (`compose_slew_word`, re-applied from the
  1 ms loop at the start/end instants by `apply_slew_edge`), and every PTP sample is DE-SLEWED by
  the scheduled displacement before either servo sees it (`DateSync::deslew_sample`) — so the
  phase lock and the rate servo read the clock as if no slew ran. Two gotchas: take the
  displacement at the WALL (`t2`), never at `t1` (a grandmaster change delivers `t1` in another
  base before the rebase); and the rate servo's phase is continuous across a fold only if the
  folded amount stays removed from its measurement (`rate_folded_ns`, kept mod 1 s because its
  phase is mod 1 s).
- **Bench:** the bit-identity pair is now two FORWARD-only runs (+8 / +20 ppm); a slew adds an
  NTP-derived rate term by design. The −15 ppm run proves the slew: no backward step, no wall
  ever running back, relative phase (each wall + its own path delay) ≤ 50 µs while slewing
  (measured 10 µs), and the words within 0.001 ppm of the forward-only run (measured 0.0004).
- **Recovering PTP time from the wall is exact to 1 ns, not better:** under the floored
  schedule two adjacent PTP instants can give the same wall. Tests compare with ± 1 ns.
- **Known limits (accepted):** a box that first hears a slew after its whole lead catches up
  with a counted LATE step, which can be backward (as any late step); the master re-aligns to
  the fleet only after a running slew ends; the local NTP fallback path (no authority heard) is
  uncoordinated and unchanged.
- **Rollout: the NTP master LAST** — a ≤ 1.9.0 follower decodes only the v1 part of the v2
  extension and would step back at the slew's start.

## Seed every simulated noise source — a statistic under an unseeded RNG fails at random

`tests/simulation_e2e.rs` drew its jitter from unseeded `rand::random()`, and its high-jitter test
asserts an AVERAGE drift rate (< 150 µs/s), a statistic of that noise. Over 60 seeds its RMS is
~50 µs/s, so the bound sat at about 3σ and roughly one CI run in a few hundred went red for no code
change (CI run 36182967772: 155 µs/s). Round 5 of #117 gave every test thread a fixed xorshift64*
seed and runs the high-jitter scenario over eight fixed seeds (the worst must pass), in parallel
because each run sleeps ~36 s of wall time. The bound did not move. Rules:

- Never widen the bound and never "re-run until green". Reproduce the failure over many seeds on
  `origin/master` AND the branch first, to tell a code regression from test noise.
- One fixed seed proves the bound for ONE noise sample. When the assertion is a statistic, run
  several seeds and assert the worst.
- A bench's own RNG (`tests/two_clock_bench.rs`) was seeded from the start. Keep it that way.

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

**CORRECTED on issue 76 — the fix below shipped as v1.8.31/v1.8.32, was verified only against a
noiseless simulation, and made a LIVE REGRESSION on strih's real hardware canary worse than the
sawtooth it replaced. Do not follow the struck-through guidance; read the corrected version after
it.**

~~The fix shape for a ramp-shaped signal: same-sign-only agreement (drop magnitude comparison
entirely), plus a "fast lane" that skips the wait for small, routine corrections (same-sign
persistence across even ONE additional sample is itself the confirming signal on a ramp — it does
not spontaneously reverse direction).~~ This assumed the master's signal is a CLEAN ramp. It is
actually genuine drift PLUS real measurement noise from whatever upstream the node is configured
against — and on a WAN upstream (issue 76: strih's own Cloudflare NTP server, `pcap_active:false`,
the less-precise userspace fallback path issue 53 built the kernel-timestamped transport to avoid),
consecutive burst offsets can scatter by MORE than the true per-check drift signal. "Small +
same-sign" is not a reliable trust signal there — the fast lane chased that noise into a step
roughly every ~10s on the live canary, worse than the original correction-lag bug.

**The corrected fix shape: NO single-sample fast lane at any magnitude — ALWAYS require
`NTP_STEP_AGREEMENT_N` same-sign agreeing samples — but replace the client's self-scaling
`max(TOL, |cand|/2)` tolerance with a FIXED, non-scaling tolerance sized to the TRUE expected
per-check accrual, not to a candidate's own (possibly noisy) magnitude.** The self-scaling formula
is wrong in BOTH directions for a signal with drift + noise: too tight when starting from a small
genuine candidate (the original issue-71 finding — causes a reset-pileup), too loose once a noisy
large candidate has already inflated it (lets a second noisy reading "confirm" the first).
A second, independent layer — excluding a burst whose own internal `spread_us` (issue 53's quality
signal) exceeds a bound from the step decision entirely — helps but is NOT sufficient on its own:
`spread_us` only measures WITHIN-burst consistency (a handful of round trips taken in under a
second); it cannot detect a systematic bias shared across an entire burst (WAN path asymmetry, a
congestion episode outlasting one burst), so a tight, low-spread burst can still carry a wrong
value. Both layers together substantially reduce, but do not fully eliminate, a small residual risk
of two independent noisy readings coincidentally landing within tolerance of each other.

**Any FIXED (non-scaling) tolerance needs its own escape valve, or it can freeze forever.** If the
true per-check accrual ever permanently exceeds the fixed tolerance (a faster oscillator than
anything measured so far, or any other sustained one-directional signal), consecutive same-sign
candidates will NEVER land within tolerance of each other — every reading CONTRADICTS the last,
agreement count never reaches the target, and the master stops stepping FOREVER, silently, with
unbounded linear error growth and no distinct alarm. This is a genuinely NEW failure class a fixed
tolerance introduces that the old self-scaling formula never had (it always eventually converged,
just with lag) — verified live in the issue-76 review by direct simulation (57ppm: 205ms of
uncorrected error after one simulated hour, zero steps). The fix: track how many consecutive
checks have passed with no actual step (regardless of WHY — tolerance never agreeing, or a
quality gate excluding every burst) and force a step past ALL gates once that count crosses a
generous bound (issue 76: 30 checks / 5 minutes at the 10s cadence — long enough that it
essentially never fires under real, even noisy, conditions, short enough to bound the worst case).
**Any new confirmation/outlier-rejection gate on a control loop in this codebase needs the SAME
question asked before it ships: what happens if the SIGNAL this gate is tuned to reject legitimately
persists longer than the gate's own patience? If the answer is "the gate rejects it forever," it
needs an escape valve — verified by actually running a simulation past the gate's own tuned
envelope, not just within it (see the section below).**

Before reusing ANY agreement/outlier-rejection mechanism on a new signal (not just the NTP step
gate — this applies to any future control-loop confirmation logic in this codebase), ask the same
question the MAD-adaptive threshold section above already asks: **is this signal noise around a
stable value, or a deterministic trend, or (as turned out to be the real answer here) BOTH at
once?** A mechanism tuned for pure noise or a pure trend is often actively counterproductive on a
signal that is genuinely a mix of the two — and any FIXED bound tuned to today's measured envelope
needs an explicit plan for what happens outside it, not just for the case that's been observed.

**A safety-net counter is only calibrated for as long as its PROXY stays coincident with the
condition it actually measures — re-verify that coincidence every time a LATER change alters the
proxy's own relationship to the real condition (issue 83's own critical review finding).** The
escape valve above counts "consecutive successful checks since the last step" as a PROXY for its
real intent, "consecutive checks the offset was over threshold but never confirmed" — under the
ORIGINAL regime (issue 76, a tight ~200us threshold with per-check accrual usually already over
it) those two things were nearly always the SAME number, so the proxy was safe and its 30-check
patience was genuinely "far longer than normal step cadence". Issue 83 then introduced a SECOND,
much larger threshold (a 25ms deadband, active while genuinely PTP-locked) whose natural
over-threshold cadence (~38-66 checks) is LONGER than that same 30-check patience — so by the time
the deadband was ever legitimately crossed, the proxy counter had ALREADY exceeded its patience
purely from checks that were never over threshold at all, and the escape valve fired unconfirmed
on literally the FIRST over-threshold sample every time, silently bypassing both the agreement gate
and the quality gate for the codebase's entire new primary use case. The fix was to scope the
counter to the REAL condition directly (reset to 0 on any under-threshold check, so it only ever
accumulates while genuinely over threshold) rather than adding a second tunable constant — this
also made the invariant correct for BOTH thresholds simultaneously, with nothing to keep in sync
by hand. **The generalizable check: whenever a control-loop safety-net counter's own doc comment
justifies its bound by reasoning about a SPECIFIC regime ("normal cadence is ~N checks, so M >> N
is safe"), and a later change introduces a SECOND regime with a different natural cadence, that
justification must be RE-DERIVED for the new regime, not assumed to still hold** — a closed-loop
simulation using a NOISELESS deterministic ramp (as issue 83's own first-draft test did) cannot
catch this, because a clean, unambiguous signal steps identically whether confirmed by 2 agreeing
samples or forced by an unconfirmed escape valve; only a noisy-outlier scenario (a single spurious
reading immediately contradicted by the next) can distinguish "genuinely confirmed" from "escape
valve fired prematurely" — see `locked_mode_escape_valve_never_fires_on_a_single_outlier_after_many_under_threshold_checks_83`
in `src/controller.rs` for the pattern.

**A hard clamp on a control-loop's OUTPUT needs a companion fix to its state-RESET logic, or a
sustained adverse input makes the residual grow UNBOUNDED instead of converging (issue 83's
second correction round).** Adding `NTP_SERVER_LOCKED_MAX_STEP_US` (a hard per-step ceiling,
5000us) closed a real safety gap — at a high enough drift rate the escape valve could otherwise
apply an oversized, unconfirmed correction — but a NAIVE version of the fix (clamp the step,
keep the EXISTING "any step resets the counter" behavior unchanged) would have been WORSE than
doing nothing: reset the counter to 0 on a CLAMPED step, and the counter needs ANOTHER full
`NTP_SERVER_MAX_CHECKS_WITHOUT_STEP` (30-check) wait before it can fire again — during which
MORE drift accrues than one clamp removed, so the residual left behind after each cycle is
strictly LARGER than the one before it (unbounded growth, verified by tracing the exact
arithmetic — `5000 removed` vs `30 checks × accrual_per_check added` — BEFORE writing any code,
not discovered by accident afterward). The actual fix pairs the clamp with a DELIBERATE asymmetry:
only a FULLY-applied step (no residual, `step_us == offset_us`) resets the counter; a clamped
(partial) step leaves it armed, so the escape valve can re-fire on the very next over-threshold
check — producing a rapid run of further clamped corrections that converges (each one removes
MORE than a single interval's new accrual) instead of one that diverges. **The generalizable
check: before shipping ANY output clamp on a control loop that has its own "confirmed vs.
forced" state machine (a candidate, a starvation counter, a cooldown), trace what happens to
that state machine's OWN reset/rearm behavior on a CLAMPED (as opposed to a full) application —
if the state resets exactly as if nothing were left over, verify by tracing the worst-case
input rate through several cycles by hand (or better, simulate it) whether removal-per-cycle
actually exceeds accrual-per-cycle. A clamp that "looks safe" (bounds one number) can silently
make the SYSTEM less safe (a different number, the accumulated residual, now grows instead of
converging) if this isn't checked.** See the `at_80ppm_past_the_tolerance_breakeven_every_step_is_hard_capped_and_the_system_converges_83`
test in `src/controller.rs` for both properties (the hard bound AND the convergence) proven
together, not just the bound alone.

**Verify a safety CLAIM (not just a numeric estimate) by actually running the real code path it
describes, never by hand-deriving from a simplified mental model of the algorithm — even when
the simplification "should" be equivalent (issue 83's second correction round, again).** A
doc comment claimed a widened tolerance "lets through the SAME 2 of 6 transitions" an existing
tolerance already accepted on a captured real WAN-noise fixture, reasoning from the raw
consecutive DELTAS between readings. The REAL gate (`ntp_step_gate`) doesn't compare consecutive
READINGS to each other — it compares each reading to the CURRENT CANDIDATE, which gets REPLACED
(not just re-measured) on every contradiction. These two models silently diverge whenever a
contradiction occurs, which this exact fixture triggers. Actually running the real
candidate/contradiction logic (not just eyeballing the deltas) found the true answer was 1-vs-2
steps, not "the same 2" — small in consequence here, but the METHOD gap is what matters: a
"verified by running" claim is only as good as whether what was RUN is the real code path, not a
paraphrase of it. When justifying a safety-relevant constant against a fixture, write the actual
test (or a throwaway script using the exact same logic) FIRST, then quote its real output in the
comment — never reason from "the deltas are X, and the tolerance is Y, so..." even when it feels
obviously equivalent.

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

**The same splicing idea, generalized to produce a genuine RED commit after the tests AND the fix
were already both written in one sitting** (issue 76's second RED/GREEN pair, for the review's own
critical finding, was built this way): `git diff <base>` the whole working change, split the unified
diff into hunks with a small Python script (`re.finditer(r'^@@', ...)`  over the patch text), sort
each hunk into "test-only" vs. "production" (a hunk entirely inside `mod tests { ... }` is
test-only; watch for hunks that mix inert NEW vocabulary — a constant, a struct field, its
zero-init — with test-only hunks when the tests need that vocabulary to even COMPILE but the
BEHAVIORAL wiring lives in a separate hunk, e.g. issue 76's `NTP_SERVER_MAX_CHECKS_WITHOUT_STEP`
constant + field had to ship in the RED commit so the test would compile, while the actual
if-condition using them stayed in GREEN), write the two hunk subsets to separate `.patch` files,
`git checkout -- <file>` back to the base, `git apply` the RED-only patch, run the tests to confirm
a genuine failure (not a compile error), commit, `git apply` the GREEN-only patch on top, and
`diff -q` the result against the originally-saved full working-tree copy to confirm it reconstructs
EXACTLY (catches an accidental hunk misclassification immediately, before it ships as a broken
commit boundary). `git apply --check <patch>` first always, on both patches, before touching the
working tree.

## When a live-hardware mystery doesn't add up, grep the EXISTING logs before adding new instrumentation

Issue 80's own investigation (strih's steady-state "drag" -- corrections that looked right in
cadence but never converged) found its root cause not by adding new diagnostics, but by grepping
what was ALREADY being logged for a completely different original purpose:
`WindowsClock::step_clock()` has always logged `[StepClock] Actual step: X (expected: Y)` (a
before/after `GetSystemTimeAsFileTime()` sanity check, presumably added to confirm the step syscall
did SOMETHING) -- nobody had ever checked whether `X` and `Y` actually MATCH. They didn't, by a
large and inconsistent margin, and that exact shortfall pattern (27.6%-116.7% delivered, no fixed
ratio) was the whole proof of a millisecond-quantization bug once checked against `SYSTEMTIME`'s
own field width. **Before reaching for new instrumentation on a live clock-daemon mystery, grep the
target's OWN log for every existing per-operation diagnostic line first** -- a value that's been
sitting there the whole time, logged for an unrelated reason, is often the fastest and most
convincing evidence available, because it was captured under REAL production conditions rather
than a synthetic repro. This generalizes past clock work too: `spread_us` (issue 53's own quality
signal) sat unconsulted by the step-decision logic for two full cycles (issues 71 and 76) before
issue 76 finally used it -- the same "an existing signal was already telling you something" shape.

## Working dantesync from a camera-box Claude session — you CANNOT compile locally at all (#557)

dantesync runs `cargo test` normally in its OWN session (it is NOT a Tier-0 repo). BUT the usual
fleet dispatch works it from a **camera-box** session (dantesync is Claude-stewarded but has no
session of its own), and camera-box's `block-tier0-local-build.sh` PreToolUse hook fires on every
Bash call keyed on the **session cwd** (the tool payload's `.cwd` = the camera-box checkout), NOT on
the `cd dantesync &&` inside your command.

**As of airuleset #557 (owner directive 2026-08-18) that hook blocks EVERY compiling cargo shape —
`build`/`test`/`bench`/`run`/`check`/`clippy`/`doc`/`rustc`, narrow OR whole-workspace, `--no-run`
or not — as an allowlist inversion (unknown subcommand fails SAFE to blocked). The old `--no-run` +
exec-the-binary workaround this section used to document is GONE (verified live on #97: `cargo test
--lib --no-run` was blocked), and the `# airuleset:build-ok` / `AIRULESET_ALLOW_LOCAL_BUILD`
bypasses are disabled for camera-box (#477).** So from a camera-box session you have NO local
compile, type-check, or test-run path whatsoever.

**Your ONLY local net is `cargo fmt --all --check`** (fmt is on the non-compiling allowlist). It is
a purely SYNTACTIC tool — it parses every file (following `#[cfg] mod` paths, so it even checks
Windows-only code), catching a stray brace / broken literal / bad token, but it does NOT type-check.
Run `cargo fmt --all` then `cargo fmt --all --check` after every edit as your parse-check.

**A second local net: a standalone `rustc` REPLICA (round 5 of #117).** The hook keys on `cargo`;
plain `rustc` on a scratch file is not a cargo shape. A PURE module (`date_offset.rs`,
`ptp_phase_lock.rs`, the bench, `time_server.rs` with `log`/`anyhow`/`libc`/`uuid` stubbed) compiles
and runs its own tests as `rustc --edition 2021 --test`, from a wrapper `mod dantesync { pub mod
date_offset; … }` in a scratch dir laid out like `src/` (symlinks keep `mod tests;` sibling files
resolving). Even the CONTROLLER runs this way: strip `serde`, stub the external crates, and drive
`tests/simulation_e2e.rs` as a binary. That is how the round-5 CI red was diagnosed: the same
seeds gave the same result on `origin/master`, on the branch and on the branch in legacy mode, so
the failure was the test's own unseeded noise, not the change. Replica results are evidence, not
proof: CI still type-checks and runs the real crate.

**Everything else is verified by CI, which is your compiler + test runner.** CI (`ci.yml`) triggers
ONLY on `push`/`pull_request` to `master`/`main` — NOT on a feature-branch push. So to actually
verify a branch, **open a PR to `master`** (that fires the `pull_request` CI); monitor it to green;
do NOT merge if your lane is implement-only (the supervisor owns integration/rollout). CI runs, in
parallel: Version Consistency, Lint (`cargo clippy -- -D warnings -A dead_code` — this is a FULL
type-check of the lib, so a green Lint proves the production code compiles + is clippy-clean; NOT
`--all-targets`, so test-code `field_reassign_with_default` is deliberately not gated), Test
(`cargo test --lib` + `--test '*'` — compiles + runs test code, the only place test-compile errors
and failing assertions surface), Build Check (Linux + **Windows** — the Windows compile is your only
proof `#[cfg(windows)]` code builds), Coverage (runs the suite under instrumentation — a green
Coverage implies the tests compiled AND passed), Security Audit. Because you cannot run tests
locally, write them with extra care (trace assertion arithmetic against the impl by hand or in a
throwaway `python3` sim) and expect CI to be the FIRST place a type mistake or a wrong assertion
shows. Match CI's gate, never a stricter self-imposed `--all-targets`.

## A master's NTP step RATE is a bounded health signal — a storm means the FREQUENCY reference is degraded (#91)

The master cannot slew (see the top of this file) — every UTC correction is a `step_clock` — so the
step RATE is a direct readout of the Dante-clock-vs-UTC frequency error, and it is BOUNDED ABOVE
while genuinely PTP-locked: at the 2500us `NTP_SERVER_LOCKED_DEADBAND_US` and the worst-ever measured
Dante-GM rate error (66ppm, #83) a healthy locked master tops out near ~72 steps/h (measured by the repo's own 66ppm locked-hour test; ~84/h by a looser hand figure). So a
*sustained* rate above that ceiling can ONLY mean the PTP frequency reference is degraded — a GM
outage makes `server_step_threshold_us` fall to the tight 200us threshold, which then step-corrects
UTC almost every 10s check → the 129-180 steps/h storm observed live on strih (#91). Two consequences:

- **A step-rate alarm is zero-false-alarm BY CONSTRUCTION.** `NTP_STEP_STORM_THRESHOLD_PER_HOUR = 120`
  sits above the ~72/h measured healthy-locked ceiling and below the observed storm floor,
  so it can only fire on a genuinely degraded frequency reference, never on healthy locked stepping.
  When picking or moving this threshold, re-derive the healthy ceiling from the CURRENT deadband and
  the max plausible GM rate — never set it below what a healthy locked master legitimately produces.
- **No servo/threshold change can fix a storm — only restoring the PTP grandmaster/frequency source can.**
  The rate tracks a REAL external frequency error the NTP loop cannot slew away; widening the deadband
  to slow it is the masking #83 already proved drops frames. The honest response is to ALARM
  (`[NTP][STEP-STORM]` log line + `/status.ntp_step_storm` / `ntp_steps_last_hour`), never to widen a gate.

**Diagnostic — read the step SIZE to tell which threshold is active.** ~200-1200us steps every ~10-20s
= the TIGHT threshold (NOT genuinely locked: degraded/absent PTP, NTP is the sole reference). ~2.5-2.7ms
steps every ~40-70s = the LOCKED deadband (healthy, chasing only the GM's own real rate error). In the
#91 storm the PEAK was small tight-threshold steps (0.35-1.2ms), while the "+2.7ms" quoted in the issue
body was the later recovering/locked phase — so grep the log for BOTH regimes before concluding which
one a reported step size represents.
