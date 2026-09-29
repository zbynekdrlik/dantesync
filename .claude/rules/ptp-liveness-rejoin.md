---
paths:
  - "src/ptp_rejoin.rs"
  - "src/controller/ptp_liveness.rs"
  - "src/controller/ptp_liveness/tests.rs"
  - "src/controller/date_sync/rejoin_tests.rs"
  - "src/net_linux.rs"
  - "src/net_pcap.rs"
  - "src/net_winsock.rs"
  - "src/traits.rs"
---

# PTP liveness: the re-join and the honest `/status` while PTP is stale (dantesync#112)

## The failure it fixes

A running node kept `mode=LOCK is_locked=true` for hours or days with ZERO PTP frames on the wire
(dev1, imag; strih-lx on 29.9.2026 after a USB NIC re-plug). Two separate causes:

- **The lock state is only changed by a sample window.** `apply_self_tuning_servo` (the only
  writer of `is_locked`) runs when a PTP window closes, so with no packet nothing ever cleared it.
  The offline edge wrote `mode = "NTP-only"` once, and the next 10 s `tick_status` wrote LOCK back.
- **The receive path was opened once.** Linux joined 224.0.1.129 on the interface resolved at
  startup; a NIC that comes back under the SAME name and IP but as a new netdev (USB replug, new
  ifindex) has lost that membership, and the socket stays bound but deaf. Windows kept a dead
  pcap handle after a NIC swap (`ERROR_DEVICE_REMOVED`).

A service restart cured both, but on the fleet's NTP master a restart re-derives the fleet date
offset `D` from its own NTP-stepped wall: 29.9.2026, −19.8 ms, every box stepped.

## The contract

- **One definition of stale:** `PtpController::ptp_stale_at` = no ALLOWED PTP packet (one that
  passed `gm_allowlist`) for more than `PTP_TIMEOUT_SECS` (10 s). The offline edge, the clock
  alarm (`sample_clock_health`), the re-join and `/status` all use it. Do not add a second
  threshold; `ptp_rejoin::REJOIN_AFTER` is pinned to it by a `const _: () = assert!(..)`.
- **While stale `/status` says so:** `is_locked=false`, `mode="NTP-only"` (the offline edge's own
  name; the tray's orange "PTP offline"; the 31900 reply's mode 5; every camera-box gate reads a
  non-LOCK/NANO mode as degraded), `settled=false`. `offset_ns` keeps the last value, flagged by
  `last_ptp_rx_age_s`. The INTERNAL servo lock (`self.is_locked`, the held learned frequency) is
  deliberately NOT cleared: when the packets return the node publishes LOCK again at once and the
  phase lock re-engages on its next window, exactly as after any brief outage. Only the published
  view is gated.
- **The re-join (`PtpNetwork::rejoin`) re-opens ONLY the receive path.** No clock call, no servo or
  measurement reset (`reset_ptp_measurement_after_step` is for clock steps), no date-offset change.
  `controller/date_sync/rejoin_tests.rs` pins it on a master in daily AND micro mode: loss → re-join
  → re-acquire keeps `D` and `date_offset_seq`; the only step is the master's own Join back onto
  the fleet line (`realign_master_to_fleet`).
- **The schedule is pure** (`ptp_rejoin::RejoinSchedule`): the first attempt when the silence
  reaches 10 s, then 30 / 60 / 120 / 300 s from the previous attempt (the last repeats), from
  scratch after the next allowed packet (`note_allowed_ptp_packet` resets it). It runs at the top
  of `process_loop_iteration`, BEFORE `recv_packet`: a dead Npcap handle errors every receive, and
  the `?` would otherwise return before the re-join.
- **Every backend implements `rejoin`** (the trait method has no default on purpose): resolve the
  interface NOW (a resolve failure keeps the old path), close the old sockets / capture FIRST (the
  new pair binds the same ports; the old membership may belong to a dead netdev), then open again
  through the SAME selection code as startup (`open_pair` on Linux, `open_ptp_capture` on Npcap —
  never a second, diverging copy). An open failure leaves no path (`recv_packet` → `Ok(None)`)
  until the next attempt. `changed` compares with the LAST SUCCESSFUL join, kept through a failed
  attempt.

## Is `D` re-derived at re-lock? (the finding, 29.9.2026)

No, not in-process. `PhaseLockCore::on_window` anchors `D` only while the anchor is `None`
(`AnchorEvent::Anchored`, the first lock). An outage disengages the core and keeps the anchor; a
re-engagement more than 1 ms off is `Realigned` (only the node's own anchor moves, the fleet `D`
and `seq` are untouched, the master steps its own wall back). Only a time-base change — a new GM
UUID or sync source, or a > 1 s jump (`DISCONTINUITY_NS`, a GM reboot) — rebases the fleet `D` by
the observed base shift, with no wall step. The fleet date moves on a RESTART, which is why the
re-join exists.

## Testing without a local compile (Tier-0)

- The pure module runs under a standalone `rustc --edition 2021 --test` replica: replace
  `use crate::traits::RejoinOutcome;` with a local struct and drop the serde derives.
- The Linux backend's tests use REAL non-blocking UDP sockets through a scripted
  `PtpSocketFactory`. 319/320 need root, so each fake socket binds an ephemeral port on its OWN
  loopback address (`127.0.1.N`): with one shared `127.0.0.1` the kernel may hand a new socket
  the port an old one just released, and a "the old address is free" probe flakes. "Closed first"
  is checked at the moment each new socket opens (`free_at_open`), not afterwards. An std-only
  `rustc` replica (an `anyhow` shim, `recv_from` in place of the nix `recvmsg`) runs them
  locally: build it from a copy with `//!` turned into `//` and the three crate uses swapped for
  local modules.
- The controller glue and the Windows backends compile only on CI: the RED push is the first
  compile. Check the RED run fails exactly the expected tests before writing GREEN.
