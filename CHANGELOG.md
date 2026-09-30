# Changelog

All notable changes to DanteSync will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.15.0] - 2026-09-30

### Fixed

- **A restart of the NTP master no longer steps the fleet date (issue #126).** On 30.9.2026 a
  `systemctl restart dantesync` on strih-lx (the master) stepped its own wall +247 ms to UTC at
  boot, started a new authority session, and every follower dropped the silent authority after
  30 s and stepped on its OWN next NTP samples: resolume +247.730 ms at 06:02:42Z, cam7 at
  06:02:46Z, dev1 +247.77 ms at 06:02:42Z — the whole rig at scattered instants, exactly the
  disturbance the nightly window exists to avoid.
  - **The master saves its date offset.** Whenever its authority's published state changes (D in
    effect, seq, a step or slew in flight, the last nightly step, the grandmaster of the anchor)
    it is written to `date-offset.json` beside `config.json` (`/etc/dantesync/`,
    `C:\ProgramData\DanteSync\`) by temp + rename, and rewritten at least every 10 minutes. A
    write error is logged and retried, never fatal. Only the master writes it; a node that starts
    as a non-master removes one left from an earlier stint as the master.
  - **A restarted master restores it.** With a readable saved state the boot step is skipped and
    the master takes no NTP step until its first PTP lock, where the state is judged: the same
    grandmaster, and the first window within `daily_emergency_ms` (5 s) of the saved D. Accepted,
    the authority continues with the same D and seq (a step whose instant passed while it was down
    is in effect) and the master re-joins ONLY its own wall (a reboot's RTC, the free-run of the
    gap); the accumulated UTC error is left to the next nightly window as if it had not restarted.
    A restore inside a step's lead takes that step on the master's own wall at its instant, with
    the fleet. Another grandmaster, a grandmaster that restarted its uptime, a wall seconds off, a
    record over a day old, or no PTP lock within 300 s: the boot step runs then and a new session
    starts (the pre-1.15 path), loudly; a boot offset beyond twice the cap (10 s), or an NTP server
    that fails to start, takes the boot step at once. `/status.date_offset_restored`.
  - **Followers hold the fleet date through the master's silence.** After the 30 s loss a follower
    that has adopted D keeps D and seq for `system.date_offset.authority_hold_s` (900 s by
    default; `0` = the 1.14 fallback): its NTP readings are report-only (the `[NTP] offset:` line
    unchanged), a step already scheduled still lands at its instant, `/status.date_authority` is
    `"holding"` with `date_authority_hold_age_s`, and the master heard again with the same D is
    re-joined with no step. Past the hold the 1.14 fallback runs.
  - **A coordinated step on request, for acceptance tests.** `POST /date/step` on the master's
    :8898, from loopback only, announces the current UTC error as ONE coordinated step two leads
    (10 s) ahead, exactly like the nightly window: `202` + `{"accepted":true,…}`. It must carry
    the `X-DanteSync-Step` header and a loopback `Host` (no web page, rebound or cross-origin, can
    send it). Refused with the reason (`409`) while another change is in flight, without a settled
    UTC estimate, or for a fleet ahead of UTC; `403` off loopback, without the header or with
    another Host; `503` when the loop does not take it within 3 s — the request is then abandoned
    and nothing is ever announced for it. Every other request still gets the status JSON.
    `/status.date_step_trigger_last`.
  - Two-clock bench: a master restart 2 h in (down 2 s, locked 65 s later) with six boxes — the
    1.14 path reproduces the incident (a +62.8 ms boot step, all five followers stepping on their
    own NTP at scattered instants); the 1.15 path keeps seq 2, steps no follower, and two steps on
    request land on all six boxes within 39 µs; a restart 1 s after a step's announce lands that
    step on every box, the master included, within 43 µs.
  - **Rollout:** the FIRST start of 1.15 on the master has nothing saved (1.14 saved nothing) and
    still takes the old path once — roll the followers first and the master within ~40 minutes
    after the nightly step (its error is then below the 50 ms boot-step threshold). **Rolling the
    master back** to a pre-1.15 build: delete `date-offset.json` with it — a 1.15 installed again
    within a day would otherwise restore the session from before the rollback, while the fleet
    followed the older build's new one.

## [1.14.0] - 2026-09-29

### Fixed

- **A grandmaster FREQUENCY step is followed in seconds, not minutes (camera-box issue 1372).**
  On 29.9.2026 the audio-VLAN Dante leader flipped four times between two devices whose clocks
  differ by ~25 ppm, and the video-VLAN grandmaster followed it under the same identity. The
  phase lock's PI is deliberately slow (critically damped, ~100 s), so every node took ~8-12
  minutes to follow each flip, with a phase error near 1 ms; for all that time every OBS media
  clock was off the Dante tick while the Dante audio devices had followed at once.
  - **The detector.** Over the last 20 s of engaged windows the phase lock fits a line to the
    OPEN-LOOP phase (the phase error minus the integral of the word it applied), whose slope is
    the oscillator's rate against the grandmaster whatever the loop commanded. The frequency the
    integrator has not learned is a step when it is at least 6 ppm, at least 6 standard errors
    of the slope, the window is LINEAR (no level shift and no slope change inside it explains it
    better, measured against the white noise of the window, so the estimate waits until the window
    is past the step), and the frequency is still there with the most likely one and two level
    shifts explained away (so a delay spike, a path-delay change, one in two stages, or an offset
    absorbed into `D` is not a step). Six consecutive windows confirm it; then nothing for 30 s.
    Steps of 6-7 ppm are followed only in part and smaller ones stay with the PI; >= ~8 ppm is
    followed reliably. What a 20 s window cannot tell from a frequency change: a path delay that
    ramps at >= ~6 us/s for >= ~15 s (at any timestamp noise), or changes in three or more
    stages. Such a false re-seed roughly doubles the phase excursion the path change causes and
    is reversed ~30 s later; one near the 6 ppm minimum may instead decay through the PI.
  - **The response.** The integrator jumps by the measured error (at most 100 ppm per event), and
    the phase the step left (~400 us for 25 ppm) is pulled back along a decaying reference
    (tau 20 s) whose rate the word carries as feed-forward, so the recovery neither disturbs the
    learned frequency nor overshoots. No wall step; the fleet date offset `D` and
    `date_offset_seq` are untouched. Without a confirmed step the frequency word is exactly the
    plain PI's. The applied rate carries that pull: right after the re-seed the word runs up to
    ~|e0|/tau (~23 ppm for a 25 ppm step) past the new frequency, in the step's direction, and
    decays with tau; its 20 s mean is within 1 ppm of the grandmaster about 95 s after the step.
  - One `[PHASE-LOCK] frequency step: +25.0ppm over 20s (fit σ ...) -- integrator re-seeded I a
    -> b ...` line per event; `/status` gains `freq_steps`, `last_freq_step_ppm` (the step
    followed; positive = the grandmaster sped up) and `last_freq_step_ts` (additive).
  - Two-clock bench, 25 ppm step at 20 us sample noise: confirmed within 22 s, the learned
    frequency within 1 ppm from 21.5 s, the phase within 50 us from 82 s, the applied word's
    20 s mean within 1 ppm from 95.5 s (the plain PI: the learned frequency after 501 s, a 928 us
    phase peak). The same with a new grandmaster (identity change + 25 ppm). No detection under
    three hours of 30 us noise with a heavy tail, a 0.1 ppm/min wander, 60-120 us path changes
    under 50 us noise, path changes in two stages at 20-30 us noise, or a 100 us absorb into `D`.
    A 300 us path ramp over 20 s (the known limit): at most the false event and its reversal;
    measured on 6 seeds, 5 re-seeded and reversed with the learned frequency back within 1 ppm by
    54 s, the sixth (not re-seeded) by 161 s like the plain PI. A six-box fleet follows a flip
    within 21 s, walls within 59 us, with frequency words bit-identical across two UTC scenarios.

## [1.13.0] - 2026-09-29

### Fixed

- **A node no longer reports LOCK with no PTP on the wire, and it re-joins PTP by itself (issue
  #112).** Before, dev1 and imag showed `mode=LOCK is_locked=true` with a frozen offset for days
  after their PTP stopped. On 29.9.2026 strih-lx, the fleet's date master, showed it for 18
  minutes after a USB NIC re-plug. Only a service restart recovered, and on the master that
  restart re-derives the fleet date offset, so every box steps.
  - **The re-join.** When no allowed PTP packet has come for 10 s (the staleness the clock
    alarm already reports), the loop re-opens the PTP receive path, then again after 30, 60,
    120 and every 300 s while it stays silent. After the next allowed packet the schedule
    starts from scratch. One `[NET] no PTP announce for Ns -- re-joining PTP multicast (attempt
    k)` line per attempt. A failed attempt is logged and retried, never fatal.
    - Linux: both sockets are closed, then joined again
      (`Joined Multicast Groups on <if> (<ip>) - Kernel timestamping (rejoin)`). A NIC that
      came back as a new netdev had lost the old membership.
    - Windows: the Npcap capture is re-opened (a fresh `Found device:` line). A swapped NIC left
      a dead handle. The device is chosen before the old capture is dropped, so a failed choice
      keeps it.
    - **Which interface.** First the one that now carries the address PTP was last received on,
      under whatever name it came back with. A re-plugged USB NIC gets a new, higher interface
      index, and the startup choice (the first interface in the list) could then be tailscale,
      docker or a bridge. Only when no interface carries that address does the startup choice
      apply (on Windows the `gm_allowlist` subnet rule still comes first).
  - **A re-join never touches the clock or the date.** The servos, the phase lock and the fleet
    date offset D stay as they are. On the master, a loss, the re-join and the re-acquisition
    keep D and `date_offset_seq`; the master only steps its own wall back onto the fleet line
    (one join step), as after any PTP outage.
  - **PTP liveness is the grandmaster's time.** Only a Sync or a Follow_Up from an allowed source
    counts; a runt datagram or another device's Delay_Req no longer keeps a node "live". The same
    rule decides `gm_source_ip`: another follower's Delay_Req no longer shows up as the
    grandmaster.
  - **`/status` is honest while PTP is stale:** `is_locked=false`, `mode="NTP-only"` (the
    tray's orange "PTP offline"), `settled=false`. The last `offset_ns` is kept; its age says it
    is old. `=== UNLOCKED === (no PTP packets for 10s)` and `=== LOCKED === (PTP packets back
    ...)` mark the edges in the journal. LOCK returns with the packets, with no restart.

### Added

- `/status`, all additive:
  - `last_ptp_rx_age_s`: seconds since the last allowed PTP packet; `null` before the first.
  - `ptp_rx_pps`: allowed PTP packets per second over the last 10 s (a Dante grandmaster sends
    about 16; it saturates at 2000 under a packet storm).
  - `rejoin`: `attempts`, `last_ts`, `last_iface`, `last_ip`, `last_error`.

## [1.12.0] - 2026-09-26

### Changed

- **The fleet date is corrected ONCE A NIGHT (issue #119, owner decision).** The NTP master used
  to correct the date continuously in 1.11: a coordinated 500 µs step every 20 s. It now reads
  UTC all day but announces nothing. When the nightly window opens it announces ONE coordinated
  step of the whole error, forward or backward, pre-announced over the usual DSYX extension
  10 s ahead.
  - **Why.** Every clock step on the stream box disturbs the Dante Virtual Soundcard: its ASIO
    input starves, and OBS ratchets its audio buffering from 85 to 405 ms. With the 1.11 cadence
    `mbc` starved 60 of 312 minutes; one step per 10 minutes starved nothing. The grandmaster (a
    PCIe card with no clock input) cannot be locked to UTC, so the date still has to be stepped.
    Now it is stepped once a night.
  - **By day** the fleet date runs at the Dante tick and drifts from UTC at the grandmaster's
    rate: about +1.5 s a day on the rig (+17.6 ppm). Every box shares that drift, so the boxes
    agree to the µs and only the date is off. `date_offset_error_ms` grows to that much by
    design.
  - **The window** opens at `system.date_offset.daily_step_utc`, default `"02:00"` UTC (04:00
    CEST / 03:00 CET; UTC keeps it free of a time-zone library, and the 1 h DST shift is
    accepted). It is read on the fleet wall. The step is made once per UTC day. When no fresh
    UTC reading exists at the window, it waits up to 30 minutes for one; otherwise the night is
    skipped (loudly) and the next night steps both days.
    - "Fresh" means at least six readings in the last 5 minutes, the newest within a minute, so
      one reading after a UTC gap never sets the whole step.
    - A window that opened and closed while another date change was in flight is reported as
      missed (loudly).
    - An emergency step re-judges the windows from where it lands, so a master that booted
      hours ahead and was stepped back still steps the next night.
  - **An emergency** (an error beyond `daily_emergency_ms`, default 5000) is stepped at once,
    loudly: a bad boot or a lost UTC reference.
  - **The master's own PTP outage.** In daily mode the master no longer steps its own wall to
    UTC while it has no PTP: that would move it up to a day's drift off the fleet. It free-runs
    on the learned frequency and still takes the fleet's nightly (and emergency) steps on its
    own wall. If one of those steps fails, it re-joins the fleet line after the 10 s backoff.
    Once PTP is back it re-joins the fleet line with one step of the measured free-run error.
  - **`system.date_offset.correction`**: `"daily"` (the new default) or `"micro"` (the 1.11
    behaviour, unchanged). The 1.10 step-bound correction (`"bound"`) no longer exists since
    1.11.0; that value, or any other, means `"daily"` with a loud warning. A bad `daily_step_utc`
    means 02:00 with a warning.
  - **Followers need no change.** They apply the nightly step like any announced step,
    counted as `coordinated`.

### Fixed

- CI Coverage: `cargo tarpaulin` runs with `--timeout 300`. Its default 60 s without a ptrace
  event is shorter than the two-clock bench's quiet stretches on already-covered code.

### Added

- `/status` (the NTP master), all additive:
  - `date_correction_mode`: `"daily"` / `"micro"`; empty on a follower.
  - `date_daily_next_utc`: the next window as an RFC 3339 UTC second, e.g.
    `"2026-09-27T02:00:00Z"`.
  - `date_daily_last_step_ts`: the fleet-wall epoch second the last nightly step lands on.
  - `date_daily_last_step_ms`: that step's signed size.
- In daily mode `date_correction_falling_behind` and `date_micro_paused` are always false.
- The pending step is visible during its lead as before, on every box: `date_step_pending_ns`
  (signed) and `date_step_due_in_ms`. They clear once it lands.
- New journal lines on the master: `nightly date step ±… ms`, `nightly date step not needed`,
  `nightly date step waiting`, `nightly date step SKIPPED` and `nightly date step MISSED`.

## [1.11.1] - 2026-09-26

### Fixed

- **A date step no longer moves the rate (issue #119, a 1.11.0 regression on Windows).** Every
  clock step lands exactly, so the PTP phase lock sees no phase error from it and the frequency
  word stays on the Dante tick.
  - **The bug.** Windows stepped the clock as "read now, set now + offset", reading "now" with
    `GetSystemTimeAsFileTime`. That clock only updates on the clock interrupt, while
    `NtSetSystemTime` sets the precise time. So every step landed short by the interrupt lag
    (0 … one clock-interrupt tick). The phase lock paid the shortfall back through the rate: on stream the error
    after each +500 µs micro-step was −170 … −860 µs, and the word sat +8 … +13 ppm off its
    baseline.
  - **Why it only showed now.** Since the 1.11.0 micro-corrections steps come every 20 s instead
    of every ~47 minutes, so the rate was never clean. The Windows media clocks that follow the
    adjustment rate (the camera-box OBS audio) left the Dante tick.
  - **The fix, on both operating systems.** Read the precise wall and a clock that no step moves
    (Windows: QPC at the system-time rate; Linux: `CLOCK_MONOTONIC`), the latter on both sides of
    the wall; a reading preempted in between is taken again. Set the target from that read plus
    the learned read→set latency (the median of the last 8 sets, so preempted sets do not count).
    Measure what the set actually did, and correct a residual beyond 10 µs, at most 4 sets. A fix
    runs forward always (a late set, either way the step went) and backward only for a backward
    step, so a forward step never runs the wall back. A move no set can make is never chased:
    another writer, a failed clock read, or a set stalled for milliseconds. A correction set that
    fails keeps the step as made, so `D` moves with the wall.
  - **Unconfirmed, and the on-rig check for it.** The stream's timer tick is not measured; a
    0.5 ms tick fits the always-negative error, which accumulated to −860 µs under the pay-back.
    After the roll, once the learned latency has settled, the `[StepClock]` lines should show
    `1 set(s)` (an occasional preempted set: 2) and a residual within 10 µs, and
    `date_step_phase_jump_us` should read a few µs.
  - **The step's log line** is now `[StepClock] stepped +500.0us (requested +500.0us, residual
    +0.0us, 1 set(s), learned set latency …, coarse clock lag …)`. The coarse lag shows what the
    old path would have lost. The old `Actual step: X (expected: Y)` line compared the coarse
    clock with itself, so it could not see the shortfall.
  - **Bench.** The two-clock bench models the Windows clock and a micro-step every 20 s for an
    hour, at 0.5 ms, 1 ms and 15.625 ms ticks, with and without the post-step grace, plus a
    Windows day with backward steps (the joins, a 3 s coordinated one). The learned rate stays
    within 0.13 ppm of the truth (it was 24 ppm), the 20 s mean word within 0.28 ppm, every step
    within 9 µs and the relative phase within 21 µs.
- **`/status` adds `date_step_phase_jump_us`**: how far the phase-lock error moved across the last
  step it could measure (the first window after it minus the last before it; not measured when
  `D` moved again in between, or across a PTP outage). An exact step reads a few µs.

## [1.11.0] - 2026-09-26

### Changed

- **The fleet date is corrected in sub-threshold MICRO-corrections (issue #119 follow-up).** Until
  now the NTP master let the fleet date drift up to the 50 ms step bound and then corrected it in
  ONE event: at the grandmaster's +1.06 ms/min drift that was a 50 ms event every ~47 minutes,
  1.5 video frames that every wall-anchored consumer saw (OBS senders re-gridded, SongPlayer
  slewed it in over minutes). Now, beyond a 2 ms dead band, the master corrects in increments of
  at most `system.date_offset.micro_step_us` (new key, default 500 µs, clamped 50–1000) no more
  than once per `system.date_offset.micro_interval_s` (new key, default 20 s, clamped 10–600):
  a capacity of 1.5 ms/min. A forward increment is a coordinated step, a backward one a
  coordinated slew at `slew_ppm`, announced two leads (10 s) ahead from the master's loop. The
  error is estimated robustly from the last 20 minutes of UTC readings (a Theil–Sen drift over
  one-minute medians, the level the median of the last 5 minutes projected along it), every band
  is widened by the readings' measured noise, and a correction against the last one needs twice
  the dead band unless the drift itself turned — so a jittery UTC path (±5 ms asymmetric in the
  bench) never makes the date oscillate. A drift beyond the capacity logs
  `date correction falling behind` loudly and sets `/status.date_correction_falling_behind`,
  never a large step; the only large correction left is an abnormal error beyond 2 × the step
  bound (100 ms: one coordinated step, either direction, logged loudly). The step bound no longer
  triggers anything else, and the slew extension is gone. Every box logs a micro-correction
  quietly (`[DATE] stepped +500us (micro, seq N)`, `[DATE] micro-slew done`) and keeps it out of
  the NTP step-storm count. `/status` adds `date_micro_active`, `date_micro_last_us`,
  `date_correction_rate_ms_per_min`, `date_correction_falling_behind` and `date_offset_micro`. The
  31900 extension is v3: flags bit 2 = MICRO (same 48 bytes; a 1.10 follower ignores the bit and
  applies the increment as a plain step or slew). Rollout: followers first, the NTP master last.
  Nothing is decided without a UTC reading in the last minute (no silent holdover: the journal says
  `micro-corrections paused` / `resumed`, `/status.date_micro_paused`); the interval is raised to
  the in-flight time of one increment (two leads; a backward one also its slew), so the capacity
  and the alarm are honest per direction; `date_offset.step_bound_ms` is floored at 5 ms.
  **Consumers:** the step bound now only sets the abnormal cap. The journal's
  `[NTP] offset: … step bound 50000us)` line and `/status.ntp_deadband_us` /
  `ntp_step_threshold_us` on the master still carry it unchanged (byte-compatible), but they no
  longer say how far the fleet date may sit off UTC — that is now the 2 ms dead band, with
  `date_correction_falling_behind` as the alarm to grade on.

## [1.10.0] - 2026-09-26

### Changed

- **A backward fleet date correction is SLEWED, not stepped, up to 2 × the step bound (issue #119).** A backward step
  runs wall time back on every box at once, and wall-time consumers lose audio: the camera-box
  stream OBS lost 43.7 ms of Dante audio at the −51 ms fleet step (camera-box#1372), while the
  forward steps lost nothing. Now only a positive correction (the fleet behind UTC) is a
  coordinated step. A negative one up to 2 × the step bound (100 ms by default) is announced as a
  coordinated slew (a larger one is an abnormal state — a master booting on a bad NTP reading — and
  is a coordinated step, logged `date correction too large to slew`): from its start instant
  (the usual ≥ 5 s lead) every box moves the fleet date offset down at `slew_ppm` (new key
  `system.date_offset.slew_ppm`, default 100 = 50 ms in 500 s, clamped 10-500) until it is paid.
  The schedule is a pure function of PTP time, so all boxes move together. The rate term is part
  of the ONE frequency word, switched at the start/end instants from the 1 ms loop. Every PTP
  sample is de-slewed before either servo reads it, so the phase lock is not disturbed. A box that
  joins mid-slew slews only the remaining part; a re-announce is idempotent; a slew in progress
  absorbs a new correction (extended only while ≥ one lead is left; a forward need waits for its
  end); the NTP master catches up with a slew its own scheduler missed (within the absorb
  tolerance, never a step). `/status` adds `date_slew_active`, `date_slew_remaining_ms`, `date_slew_ppm`
  (+ `date_slew_from_ns` / `date_slew_to_ns`); the journal logs `[DATE] slew START` and
  `[DATE] slew DONE`. The 31900 extension is now v2 (flags bit 1 = slew, its ppm in bytes 2-3,
  its start offset appended): the DSYX reply is 112 bytes against the 64-byte padded request.
  **Rollout: the NTP master LAST** (a ≤ 1.9.0 follower reads a slew as a backward step).

## [1.9.0] - 2026-09-25

### Changed

- **PTP phase lock: rate AND phase from the Dante grandmaster, NTP only moves the date (issue
  #117), the new DEFAULT (`system.clock_discipline = "ptp_phase_lock"`).** The old PTP servo was
  rate-only (`initial_epoch_offset_ns` was never read, NANO ignored < 0.1 µs/s), so cross-box wall
  agreement was held by NTP, which since #97's `phase_slew` steered the RATE up to ±5-19 ppm away
  from the Dante tick. New `src/ptp_phase_lock.rs`: once PTP-locked, a critically-damped PI on
  `e = (t2 − t1) − D` owns the frequency word, taken over bumplessly from the rate servo. `D` is
  re-anchored with no wall step on a grandmaster change. `phase_slew` is ignored (with a warning)
  under the phase lock; `clock_discipline = "legacy"` restores the pre-1.9.0 behaviour.

### Added

- **Fleet date-offset authority + coordinated steps (issue #88).** New `src/date_offset.rs`: only
  the NTP master reads UTC. It announces a new `D` when |UTC − wall| > 50 ms (2 agreeing readings)
  with an effective instant ≥ 5 s ahead, and every box, the master included, steps at exactly
  that instant. The announce rides a versioned extension of the UDP 31900 time-query reply,
  requested with `"DSYX"` (zero-padded to 64 bytes, so the 104-byte reply never amplifies it; a
  shorter `"DSYX"` is ignored). A `"DSYN"` request still gets the byte-identical 64-byte reply, and
  an older server never answers `"DSYX"`, so a new box on an old master keeps its local NTP date
  path. An older WINDOWS master logs a socket error per such poll (its 8-byte read buffer fails
  the padded request), so the NTP master is upgraded right after the canary
  (`.claude/skills/dantesync-deployment.md`, step 4). Followers poll their NTP server's
  31900 once per second on a background thread; a host that has never answered is polled every
  30 s after a minute. Tuning: `system.date_offset.{step_bound_ms, step_lead_ms}`.
- `/status` (additive): `clock_discipline`, `rate_source`, `ptp_phase_locked`,
  `ptp_phase_error_us`, `date_authority`, `date_offset_ns`, `date_offset_seq`,
  `date_offset_effective_ptp_ns`, `date_step_pending_ns`, `date_step_due_in_ms`,
  `date_offset_error_ms`, `date_step_bound_ms`, `last_date_step_{ns,ts,kind}`, `date_steps_late`.
  On the master, `ntp_deadband_us` / `ntp_step_threshold_us` report the authority bound.
- `tests/two_clock_bench.rs`: 6 boxes, a grandmaster change (the master notices it last) and a
  reboot of the new grandmaster under the same UUID (the master notices it first), UTC at +8 /
  −15 ppm vs the GM, 24 simulated hours, with and without the controller's 2 s post-step grace,
  run on the production pure modules, plus a 3 s first step, master-only PTP outages (10 min,
  3 h) and a grandmaster change during one. Walls agree within 54 µs (with a 33 µs path-delay
  spread; the live latency spread is still to be measured by the canary), and within 155 µs while
  the fleet settles that double fault (bounded at 300 µs). The rate matches the current GM
  within 0.002 ppm per hour. Every date step is coordinated (landing spread ≤ 41 µs, 0 late), no
  step happens at a grandmaster change or reboot, and replies in another time base are refused.
  The frequency LAW's commands are bit-identical across the two UTC scenarios.
- Safety of the announce: the extension names the anchor's grandmaster and carries the
  replier's PTP "now" (from its D in effect). A follower adopts only in the same PTP time base
  (`same_time_base`), and only replies from the polled address with an unpredictable request id
  are accepted. A follower returns to the local NTP path after 30 s without an applicable reply,
  keeping any step it already scheduled. One box's fault never moves the fleet D: a master
  without PTP runs its local NTP path on its own wall, then steps back onto the fleet line when
  PTP returns, and a failed master step is retried after a 10 s backoff.

### Fixed

- `tests/simulation_e2e.rs` drew its jitter from unseeded `rand::random()`, so its high-jitter
  average-rate assertion (a statistic of that noise, bound at about 3σ) failed at random, for no
  code change. Every test thread now uses a fixed xorshift64* seed, the high-jitter scenario runs
  eight fixed seeds and asserts the worst, and the bound is unchanged. `rand` is no longer a
  dev-dependency.

## [1.8.47] - 2026-08-19

### Added

- **`phase_slew` PI phase servo — bounded micro-slew instead of NTP micro-steps (issue #97),
  DEFAULT OFF.** New `src/phase_slew.rs` pure PI servo that corrects sub-50ms UTC phase error by
  a bounded, rate-limited frequency slew (`f_phase`) composed with the PTP frequency word into one
  composite word `f_total = f_ptp + f_phase`, instead of a discrete `step_clock`. The two servos on
  one clock are made safe by **feed-forward decoupling**: the commanded `f_phase` is subtracted from
  every PTP rate observation, so the PTP frequency servo never reads the slew as grandmaster
  disagreement and cannot fight it. The step path stays for cold boot / faults / `|e| > 50ms`,
  loudly logged and separately counted. Phase-servo telemetry (`e`, `f_phase` P/I split, `f_ptp`,
  saturation) is published to `/status` (additive fields) and the journal, with a "slew saturated"
  alarm when `f_phase` is capped AND `|e| > 10ms` for 60s. Gated behind `system.phase_slew.enabled`
  (default **false**) — with the flag off the frequency- and step-paths are byte-for-byte the prior
  behaviour, so merging changes nothing on the fleet until a box opts in during the canary rollout.

## [1.8.46] - 2026-08-18

### Fixed

- **Locked-master NTP steps now stay inside the proven 2500us band (issue #94).** While a
  server-mode master is genuinely PTP-locked, its periodic UTC step chases the Dante
  grandmaster's own real, unfixable rate error vs UTC (~23ppm live, 66ppm worst-ever). The
  realized step size is the offset at *confirmation* time — the trigger plus up to two 10s
  check-intervals of accrued drift — so with the previous 2500us locked trigger the realized
  step overshot to +2.4..+3.7ms at 23-66ppm, ABOVE the ≤2.5ms band camera-box proved absorbed
  (PR #1017 E2E green + the A/V-sync dock LOCKED 87min). That overshoot showed up as
  fleet-visible judder on every output. The fix lowers the locked trigger
  (`NTP_SERVER_LOCKED_DEADBAND_US` 2500 -> 1000) so the realized confirmed step stays ~1980us at
  66ppm / ~1380us at 23ppm, and tightens the locked hard step cap
  (`NTP_SERVER_LOCKED_MAX_STEP_US` 5000 -> 2500) so *no* locked step can exceed the proven band
  even under WAN noise or the >75ppm escape valve (residual worked off via the existing armed-
  counter convergence path). Steps become slightly smaller and more frequent — gentler on frame
  absorption. The issue-91 step-storm alarm is UNCHANGED (120/h threshold, trailing-hour count,
  counting semantics all byte-identical); the not-locked tight-200us path is untouched.

## [1.8.45] - 2026-08-18

> 1.8.44 is intentionally skipped — it is reserved for the concurrently-pending
> `fix-1073-review-hardening` branch, so #91 took the next free number.

### Added

- **Step-storm alarm on the fleet NTP master (issue #91).** When the PTP grandmaster goes
  unreachable, the master correctly falls out of genuine lock and `server_step_threshold_us`
  drops to the tight 200us threshold, which against a real oscillator-vs-UTC frequency error
  step-corrects UTC every ~10s check — 129–180 steps/h, live-observed on strih — and the whole
  NTP fleet chases each step (fleet-wide frame skips). Because the NTP loop cannot slew a real
  frequency error away (PTP owns frequency), no threshold change can remove this; the honest fix
  is to make the degradation LOUD. The master now tracks its own trailing-hour step count and,
  when a server-mode node exceeds `NTP_STEP_STORM_THRESHOLD_PER_HOUR` (120/h — comfortably above
  the ~72/h measured ceiling of healthy locked stepping at the worst-ever 66ppm GM rate), emits a loud,
  grep-able `[NTP][STEP-STORM]` warning (rate-limited) and sets `/status.ntp_step_storm`. The
  raw metric is published as `/status.ntp_steps_last_hour` (the "steps/h" health signal issue
  #67 asked for), both additive fields a dev1 watchdog can poll. This closes the 19h-silent gap
  that let the storm run unalerted. No step threshold was widened.
## [1.8.44] - 2026-08-17

### Fixed

- **Multi-homed PTP interface selection — adversarial-review hardening (camera-box issue 1073
  follow-up to 1.8.43).** The 1.8.43 selector is correct for the deployed `/24` allowlist; this
  patch closes two review-flagged edge cases and adds the missing determinism coverage:
  - An **over-broad `gm_allowlist`** (e.g. a `/16` that spans BOTH the rig and the mbc subnet) used
    to match two distinct NICs equally and let pcap enumeration order silently pick one — which could
    flip a previously-working box to the wrong NIC. DanteSync now DETECTS that ambiguity, keeps the
    OS default interface (never worse than before), and warns loudly to narrow the allowlist.
  - The multicast IGMP join now uses the **exact allowlist-matched address** (not the device's first
    IPv4), so on a multi-IP NIC the join and the selection log always agree.
  - Added tie-break determinism tests (an exact tie keeps the first-listed interface; the
    interface-prefix-length secondary key is proven load-bearing against a wide-mask NIC).

  Backward compatibility is unchanged from 1.8.43: an empty/unrestricted allowlist, or no interface
  on a trusted subnet, keeps the historical default-interface behavior byte-for-byte.

## [1.8.43] - 2026-08-16

### Fixed

- **A multi-homed box now attaches its PTP capture to the grandmaster's NIC (camera-box issue
  1073, second half).** The Windows PTP receive path inherited the OS default interface
  (`net::get_default_interface` → first non-wireless bindable NIC) for BOTH the pcap capture and the
  IGMP `224.0.1.129` join. On the dual-homed stream box (rig `Ethernet` `10.77.9.204` + mbc
  `Ethernet 2` `10.77.7.204`) that was the mbc NIC, so the box never joined the group on the rig NIC
  and never captured the rig grandmaster `10.77.9.184` — it only saw the foreign `10.77.7.x` PTP
  (correctly dropped by the 1.8.42 `gm_allowlist`, leaving the box in NTP fallback with no
  grandmaster at all). DanteSync now picks the capture/join interface whose subnet is on a trusted
  `gm_allowlist` prefix (`GmAllowlist::select_interface`), so both attach to the rig NIC and the box
  receives `10.77.9.184`. This mirrors the dual-homed interface selection already proven for the NTP
  transport in 1.8.x (`find_device_for_ntp_server`).

  **Backward compatible — no config change for anyone already working.** An empty/unrestricted
  `gm_allowlist` (the default), a `/0`-only allowlist, or a box with no interface on a trusted
  subnet keeps the historical default-interface behavior byte-for-byte, so single-homed boxes are
  unaffected. Only a **multi-homed** box **with** a restricting `gm_allowlist` whose default
  interface is not on the grandmaster's subnet changes behavior — its PTP capture/join moves to the
  grandmaster-subnet NIC. The Linux socket receive path is unchanged.

## [1.8.42] - 2026-08-16

### Fixed

- **A foreign-subnet grandmaster can no longer steal the PTP lock (camera-box issue 1073).** The
  PTP client had no best-master election: it adopted the source of the last Sync packet the capture
  saw (last-writer-wins), so a node that also sees a foreign subnet's PTP multicast could silently
  lock onto the wrong grandmaster. Live incident: the stream box (rig `10.77.9.x`) also sees mbc's
  `10.77.7.x` and locked onto `10.77.7.109` instead of the rig grandmaster `10.77.9.184`.

### Added

- **`system.gm_allowlist` — a trusted grandmaster-source allowlist.** Each entry is an exact IPv4
  (`"10.77.9.184"`) or a CIDR prefix (`"10.77.9.0/24"`); a PTP packet whose source IP is not
  permitted is dropped as-if it never arrived. **EMPTY (the default, and every existing config) =
  UNRESTRICTED — accept any source, unchanged behavior**, so a single-GM network needs no change.
  Parsing is fail-open (an all-invalid list degrades to unrestricted with a loud startup warning),
  and a valid-but-wrong allowlist is made diagnosable (a rate-limited drop warning, and the
  PTP-offline log distinguishes "grandmaster absent" from "grandmaster blocked by the allowlist").

  To restrict a box to the rig subnet, add to its `config.json` (Linux `/etc/dantesync/config.json`,
  Windows `C:\ProgramData\DanteSync\config.json`) and restart the service:

  ```json
  "system": { "gm_allowlist": ["10.77.9.0/24"] }
  ```

## [1.8.29] - 2026-08-11

### Fixed

- **The NTP master no longer free-runs from boot (#68).** In `ntp_server_mode` DanteSync synced to
  its upstream exactly ONCE at service start and then disabled periodic queries ("this machine IS
  the time source"). That is true of the fleet's mutual coherence and false of UTC: the clock is
  frequency-locked to the Dante grandmaster, whose rate is not UTC's, so the master's UTC phase
  error integrated from boot with nothing subtracting from it — 6-19 ppm measured on the live rig,
  i.e. ~21 ms nineteen minutes after a restart and 1.04 s over two days, with the whole fleet
  coherently following it. The master now keeps re-querying upstream and disciplines itself,
  reusing the same threshold + two-agreeing-samples + step machinery every client node uses.
  Restarting the service is no longer the remedy for drift.
- **A benign UDP reset no longer stalls the NTP server.** On Windows, `WSAECONNRESET` (os error
  10054) is raised routinely on a server socket when an earlier reply drew an ICMP
  port-unreachable. It was treated as an unexpected error — logged at error severity and followed
  by a 100 ms sleep during which the fleet's time source answered nobody. It is now classified as
  benign and skipped immediately, and on Windows `SIO_UDP_CONNRESET` is disabled at socket creation
  so the stack stops raising it at all.
- **`ntp_failed` reflects freshness, not only explicit query errors.** It previously had two
  writers, both inside the query path, so a node that had stopped querying reported `false`
  indefinitely. It is now also raised when no successful measurement lands within
  `system.ntp_stale_secs` — and going stale now also ARMS the query, so a node whose PTP never
  reaches lock keeps tracking UTC and recovers by itself instead of latching the alarm forever.
- **The boot-time NTP sync publishes its measurement,** and a corrected offset is published as the
  RESIDUAL. Neither happened before: nineteen minutes after a restart that measured +1.039 s and
  stepped the clock by it, `/status` still served `ntp_offset_us: 0, ntp_sample_count: 0`.
- **The served NTP reference timestamp tracks the last real upstream sync** (and is stamped with
  the measured UTC instant, so it can never advertise a reference in the future, which RFC 5905 has
  conforming clients discard).
- **A malformed `ntp_server_mode` in `config.json` no longer aborts startup** — the version
  migration checks the value is an object before indexing it.

### Added

- `/status` (and the named pipe) gain `ntp_updated_ts` (epoch second of the last successful NTP
  measurement, `0` = never) and `ntp_age_s` (seconds since; `null` = never measured). Additive
  only — no field renamed or removed, and pre-existing status JSON still parses. Grade these before
  trusting `ntp_offset_us`: `updated_ts` is written by the PTP loop and says nothing about the NTP
  fields beside it.
- `ntp_server_mode.max_step_us` (default 100 000 µs) — upper bound on a single server-mode UTC
  correction, so one wrong-but-consistent upstream reading cannot move the whole fleet at once.
  Never fires in steady state; a 1.04 s error is worked off over ~10 minutes unattended. `0` =
  unbounded. The boot-time sync is never bounded.
- `system.ntp_stale_secs` (default 180 s = 6× the query cadence, floored at one cadence) — the
  freshness window above.
- The NTP server runs supervised: an unexpected loop exit — including a panic — re-binds and
  restarts it, loudly, carrying its status source across.

### Note for operators

In server mode the master now takes small periodic steps it never took before (~0.7 ms every
~2 min at 6 ppm, ~1.7 ms every ~1.5 min at 19 ppm), and each propagates to the fleet one or two
client intervals later. That is the deliberate trade for UTC error that no longer grows without
limit; `max_step_us` is the lever if it ever matters.

## [1.8.18] - 2026-07-10

### Added
- HTTP status endpoint (`GET http://<host>:8898/status`), bound to the LAN interface, serving
  the same status JSON the named pipe (`\\.\pipe\dantesync`) already emits — lets CI/automation
  read PTP/NTP lock status over the network without a human or an SMB/pipe bridge (#47). Enabled
  by default; configurable via `http_status.enabled` / `http_status.port` in `config.json`. The
  named pipe is unchanged.

## [1.8.0] - 2025-12-28

### Changed
- **BREAKING:** Renamed project from "DanteTimeSync" to "DanteSync"
  - Main binary: `dantetimesync` → `dantesync`
  - Tray binary: `dantetray` → `dantesync-tray`
  - Windows service: `dantetimesync` → `dantesync`
  - Install directory: `C:\Program Files\DanteTimeSync` → `C:\Program Files\DanteSync`
  - Config directory: `C:\ProgramData\DanteTimeSync` → `C:\ProgramData\DanteSync`
  - Linux config: `/etc/dantetimesync` → `/etc/dantesync`
  - Named pipe: `\\.\pipe\dantetimesync` → `\\.\pipe\dantesync`
  - GitHub repository: `zbynekdrlik/dantetimesync` → `zbynekdrlik/dantesync`

## [1.7.5] - 2025-12-28

### Changed
- Installer now shows version from GitHub release (single source of truth: Cargo.toml)
- Added CI check to prevent hardcoded versions in scripts

## [1.7.4] - 2025-12-28

### Fixed
- Removed outdated hardcoded version from installer script (was showing v1.6.2)

## [1.7.3] - 2025-12-28

### Changed
- Version bump to test update badge notification feature

## [1.7.2] - 2025-12-28

### Added
- Update badge on tray icon: orange dot in corner when new version available
- Start Menu shortcut: "DanteSync" now appears in Windows Start Menu for easy access

### Fixed
- Tray icon now visually indicates when update is available (persistent badge)

## [1.7.1] - 2025-12-28

### Fixed
- Tray menu now shows "Start Service" when service is stopped (was always showing "Stop Service")
- Restart Service menu item is disabled when service is already stopped

## [1.7.0] - 2025-12-28

### Added
- Automatic update check: tray app periodically checks GitHub for new versions (every 6 hours)
- Update notification: toast notification when new version is available
- Upgrade menu item: one-click upgrade via PowerShell IRM from tray menu
- Version comparison logic to detect newer releases

### Dependencies
- Added reqwest HTTP client for GitHub API communication

## [1.6.4] - 2025-12-27

### Fixed
- Jitter estimator now persists across NTP step corrections (was incorrectly clearing on each step)

## [1.6.3] - 2025-12-27

### Added
- Adaptive jitter smoothing for high-jitter systems (Realtek NICs, Hyper-V hosts)
- JitterEstimator measures stddev of drift rate over 30-sample window
- Dynamic EMA alpha: 0.3 for low jitter (<2 µs/s) → 0.1 for high jitter (>8 µs/s)
- Jitter logging every 50 samples when adaptive smoothing is active

### Changed
- EMA alpha now adapts based on measured jitter level instead of fixed value

## [1.5.6] - 2025-12-26

### Added
- PTP offline detection: graceful fallback to NTP-only sync when PTP masters are unavailable
- Orange tray icon for NTP-only mode (PTP offline)
- Toast notifications for PTP offline/restored transitions
- NTP failure tracking with tray notifications when NTP server is unreachable
- Windows Add/Remove Programs registration in installer
- Unit tests for PTP offline detection

### Changed
- Tightened NTP step threshold from 2000µs to 500µs for better UTC alignment

### Fixed
- Application no longer hangs when PTP Dante masters are switched off

## [1.5.5] - 2024-12-24

### Added
- Grandmaster switch detection with sync source tracking
- Soft reset on grandmaster switch to preserve learned frequency
- Unit tests for sync source change detection
- CI coverage reporting with Codecov integration
- Npcap SDK checksum verification in CI
- Unit tests for net.rs (interface selection, socket binding, wireless detection)
- Unit tests for clock/windows.rs (PPM conversion math, adjustment calculation)
- Unit tests for net_pcap.rs (timestamp conversion, packet structure validation)
- Unit tests for net_winsock.rs (QPC math, control message parsing, constants)

### Changed
- Improved installer version handling (dynamic extraction from binary)
- Increased codecov patch target to 70%

### Fixed
- RwLock poison handling in IPC server (prevents service crash)
- UTF-16 allocations moved outside IPC loop (performance improvement)
- Tray icon ghost on exit

## [1.5.4] - 2024-12-23

### Added
- NANO mode exit hysteresis (require 5 consecutive samples above threshold)

## [1.5.3] - 2024-12-23

### Fixed
- was_nano initialization in tray app

## [1.5.2] - 2024-12-23

### Added
- NANO mode cyan icon and notification to tray app

## [1.5.1] - 2024-12-23

### Changed
- NANO mode: show drift in nanoseconds, lower entry threshold

## [1.5.0] - 2024-12-23

### Added
- NANO mode for ultra-precise sub-microsecond systems
- Single-instance check to dantetray

## [1.4.8] - 2024-12-22

### Added
- BPF filter for PTP packets to reduce DVS conflict

## [1.4.7] - 2024-12-22

### Fixed
- Disabled promiscuous mode to fix DVS coexistence

## [1.4.6] - 2024-12-22

### Changed
- Faster ACQ mode, cleaner Windows logs
- Disable W32Time in installer

## [1.4.5] - 2024-12-22

### Changed
- Move FreqMeasure warning to debug level

## [1.4.4] - 2024-12-22

### Changed
- Unified logs, faster ACQ, show version in install.sh

## [1.4.3] - 2024-12-22

### Changed
- Simplified config and fixed service control
