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

- **One definition of stale:** `PtpController::ptp_stale_at` = no ALLOWED PTP packet for more
  than `PTP_TIMEOUT_SECS` (10 s). An allowed PTP packet is a **time message** — `ptp::is_time_message`,
  a whole PTPv1 header whose control is Sync or Follow_Up (the grandmaster's time) — from a source
  `gm_allowlist` allows. That ONE predicate gates, in one block of `process_loop_iteration`, PTP
  liveness (`note_allowed_ptp_packet`), the grandmaster's source IP (`gm_source_ip`) and the
  dropped-foreign-GM count reset; and it gates every backend's home-address write. A runt, another
  follower's Delay_Req (with the default empty allowlist every source is allowed; every PTPv1
  follower multicasts Delay_Req to 319) or any stray datagram never counts, never poses as the
  grandmaster and never moves the home. The offline edge, the clock alarm (`sample_clock_health`),
  the re-join and `/status` all use this staleness. Do not add a second threshold;
  `ptp_rejoin::REJOIN_AFTER` is pinned to it by a `const _: () = assert!(..)`.
- **While stale `/status` says so:** `is_locked=false`, `mode="NTP-only"` (the offline edge's own
  name; the tray's orange "PTP offline"; the 31900 reply's mode 5; every camera-box gate reads a
  non-LOCK/NANO mode as degraded), `settled=false`. `offset_ns` keeps the last value, flagged by
  `last_ptp_rx_age_s`. The INTERNAL servo lock (`self.is_locked`, the rate servo's lock with its
  gradual unlock; the learned frequency is held) is deliberately NOT cleared: when the packets
  return the node publishes that lock again at once, as the decided design says ("LOCK returns
  with the packets"), and the phase lock re-engages on its next window. No post-outage window
  re-validates it first — the published LOCK is the servo's lock, as always; only staleness gates
  it.
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
- **Every backend implements `rejoin`** (the trait method has no default on purpose): select the
  interface NOW (a selection failure keeps the old path), close the old sockets / capture FIRST
  (the old membership may belong to a dead netdev, and two pairs are never open at once; the old
  one was deaf anyway, a re-join only runs while stale), then open again through the SAME code as
  startup (`open_pair` on Linux, `open_ptp_capture` on Npcap — never a second, diverging copy). An
  open failure leaves no path (`recv_packet` → `Ok(None)`) until the next attempt. `changed`
  compares with the LAST SUCCESSFUL join, kept through a failed attempt.
- **Which interface: the home address first (review round 1).** `net::get_default_interface`
  returns the first bindable IPv4 in the kernel's listing order (≈ ifindex). A re-plugged USB NIC
  comes back as a new netdev with a new, HIGHER ifindex and often a new name (strih-lx:
  `enx6c1ff766154b` → `enx002427159965`, same 10.77.9.202), so on a box that also has tailscale0,
  wg0, docker0 or bridges (dev1) the resolver answers one of those and every re-join "succeeds"
  there. So each backend keeps a **home address** — the startup address, then the address of any
  join that RECEIVES a packet (updated in `recv_packet`) — and a re-join first joins the interface
  that carries it now (`net::interface_with_ip` on Linux/Winsock; on Npcap `device_with_ip`, after
  the issue-1073 trusted-subnet rule and before the name fallback — the pure order is
  `net::choose_capture_device`, one `select_ptp_device(hint, allowlist, home)` serves startup
  (`home = None`) and re-join). Only when no interface carries it (a DHCP move) does it fall back
  to the startup resolver. Only a TIME MESSAGE moves the home (review round 2): the Linux/Winsock
  sockets bind INADDR_ANY:319/320, so a runt or a Delay_Req can reach a fallback join (tailscale0
  while the NIC is unplugged), and making that the home would pin every later re-join there. On
  Npcap the device is selected BEFORE the old capture is dropped.
- **Known limit (not in this change):** a DHCP move to a new address on a multi-interface Linux box
  still falls back to the listing-ordered resolver, like a restart does. Choosing by the trusted
  grandmaster's subnet on Linux (the Windows issue-1073 rule) needs the allowlist-filtered
  grandmaster address in the backend, i.e. a change of the decided `rejoin` signature.

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
  loopback address, taken from ONE process-wide counter (`127.0.x.y`; tests run in parallel
  threads): with one shared `127.0.0.1` the kernel may hand a new socket the port an old one just
  released, and a "the old address is free" probe flakes. "Closed first" is checked at the moment
  each new socket opens (`free_at_open`), not afterwards. An std-only `rustc` replica (an `anyhow`
  shim, `recv_from` in place of the nix `recvmsg`, a `net` stub with `interface_with_ip`) runs them
  locally: build it from a copy with `//!` turned into `//` and the crate uses swapped for local
  modules.
- Controller tests cannot move an `Instant` forward, so a "went quiet N s ago" state is made by
  re-writing the receive history IN TIME ORDER (a fresh `RxWindow`, then one packet N s ago —
  `went_quiet`), never by recording a backdated packet after a current one: the window would count
  the current one and report a rate while stale.
- The controller glue and the Windows backends compile only on CI: the RED push is the first
  compile. Check the RED run fails exactly the expected tests before writing GREEN.
