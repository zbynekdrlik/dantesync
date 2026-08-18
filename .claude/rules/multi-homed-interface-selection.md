---
paths:
  - "src/net.rs"
  - "src/net_pcap.rs"
  - "src/gm_filter.rs"
---

# Multi-homed interface selection for the PTP receive path (camera-box issue 1073)

On a DUAL-HOMED box (e.g. the stream box: rig `Ethernet` 10.77.9.204/24 + mbc `Ethernet 2`
10.77.7.204/24) the PTP receive path must attach BOTH the pcap capture AND the IGMP `224.0.1.129`
join to the NIC that reaches the grandmaster — not whichever NIC the OS enumerates first.

## Where the pieces live

- `net::get_default_interface()` — first non-wireless bindable IPv4 NIC (the OS default). This is the
  FALLBACK / default-interface hint, and on a multi-homed box it can be the WRONG NIC.
- `NpcapPtpNetwork::new(interface_name, gm_allowlist)` (`net_pcap.rs`, Windows-only) — the real
  Windows PTP capture. It calls `find_ptp_capture_device(gm_allowlist, fallback_name)` which
  enumerates pcap devices, asks `gm_filter` which NIC is on the trusted grandmaster subnet, and
  drives BOTH the IGMP join (`device_ipv4` / the matched IP) AND the capture off the chosen device.
- `gm_filter::GmAllowlist::{select_interface, best_interface_matches}` — the PURE selection layer.

## The pattern: reuse the #53 NTP dual-homed selector, don't reinvent

The identical "PTP and NTP live on different subnets on a dual-homed host" problem was already solved
for the NTP transport by `net_pcap::find_device_for_ntp_server` + `ntp_packet::select_ntp_pcap_device`
(dantesync#53 — subnet containment, longest-prefix, skip zero-netmask junk adapters). The PTP
interface selection MIRRORS it. If you touch the receive path, follow that pattern; do not add a
config field pinning the interface (rejected — the `gm_allowlist` the operator already sets IS the
signal, zero extra config).

## The pure/Windows split — test the LOGIC in gm_filter, compile-check the glue

`net_pcap.rs` is `#[cfg(windows)]` — a plain Linux `cargo test` SKIPS it, and it needs real pcap
devices to exercise. So the selection LOGIC lives in `gm_filter.rs` (pure, no pcap, no cfg) and is
fully Linux-unit-tested; `find_ptp_capture_device` is thin glue that only enumerates devices and
delegates. To verify a receive-path change:
- Logic: `cargo test --lib` (the `gm_filter::tests::*interface*` / `*tie*` / `*ambiguous*` tests).
- Windows glue: `cargo check --target x86_64-pc-windows-gnu --bin dantesync --tests --lib` (see
  `windows-only-code.md`). It compiles net_pcap but does not run it — CI's Windows leg runs the
  `mod tests`.

## Selection semantics (gm_filter)

- `overlaps` is a SYMMETRIC shorter-mask test, so a trusted prefix matches an interface whether the
  allowlist entry is an exact GM (`10.77.9.184/32` — the rig `/24` interface contains it) or a CIDR
  (`10.77.9.0/24` — the interface's own IP is inside it). A `/0` entry gives no interface signal and
  is ignored for selection (it still counts for source filtering).
- Ranking: most-specific trusted prefix, then longest interface prefix, then first-listed. The
  secondary interface-prefix key is LOAD-BEARING (a `/32` GM entry also "overlaps" a wide `/8` NIC on
  the top bits; only the longer interface prefix keeps the rig `/24` winning).
- `best_interface_matches` returns ALL best-scoring candidates so the caller can DETECT ambiguity;
  `select_interface` is just its `.first()`. `find_ptp_capture_device` treats >1 distinct matched
  DEVICE (an over-broad allowlist spanning two subnets) as ambiguous → keeps the default interface
  (never worse than pre-change) + warns to narrow the allowlist. A unique match is the normal case.

## Backward compatibility (mandatory)

Empty/unrestricted allowlist, `/0`-only, or no interface on a trusted subnet → the pure selector
returns None/empty → `find_device(fallback_name)` = the historical default-interface behavior,
BYTE-IDENTICAL. Only a multi-homed box WITH a restricting allowlist whose default NIC is off the GM
subnet changes behavior. The Linux socket path (`RealPtpNetwork`) is intentionally NOT changed (the
fleet's only multi-homed box is Windows).
