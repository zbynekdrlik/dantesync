---
paths:
  - "src/dscp.rs"
  - "src/ntp_server.rs"
  - "src/net_pcap.rs"
  - "src/ntp.rs"
---

# DSCP marking of timesync sockets (#52)

DSCP marks timesync UDP so loaded venue switches prioritise it (the MikroTik CRS
switches honour DSCP in hardware, TRUST-L3; Dante marks its own PTP the same way).
`src/dscp.rs` is the whole feature: a `system.dscp = {enabled, dscp}` config
(default ON, EF/46), a PURE `decide()`/`dscp_to_tos()` selection, and a thin
`apply()` syscall seam. **The value is a config key** — the fleet can pick EF (46)
or CS7 (56, Dante's clock queue) without a code change (open owner decision at time
of writing).

## TOS byte, not DSCP

The IP TOS/DS octet is 8 bits: DSCP is the HIGH 6 bits, ECN the low 2. So the byte
programmed into `IP_TOS` is `dscp << 2` (ECN=0): EF/46 → 184 (0xB8), CS7/56 → 224
(0xE0). DSCP is a 6-bit field, so valid range is `0..=63`; an out-of-range value
FAILS OPEN (logged, socket left unmarked) — marking is best-effort and must never
take the clock offline (same discipline as `gm_allowlist`).

## Which sockets can actually be marked (coverage split — non-obvious, verify before extending)

`setsockopt(IP_TOS)` only marks a socket we own a HANDLE to:

| Path | Platform | Markable? |
|---|---|---|
| NTP server reply (`ntp_server.rs`) | Linux | YES — effective (only when `ntp_server_mode.enabled`, i.e. the master) |
| NTP client request (`PcapNtpTransport`, `net_pcap.rs`) | Windows only | socket owned, but Windows FILTERS IP_TOS → `apply()` is a logged no-op there |
| NTP client request (`rsntp::SntpClient`, `ntp.rs`) | Linux | NO handle — socket created internally by rsntp; UNMARKABLE |
| PTP Sync/FollowUp | all | inbound-only (dantesync is a PTP slave to the Dante GM) — nothing outbound to mark |

Consequences for future work:
- **`net_pcap.rs` / `PcapNtpTransport` is `#[cfg(windows)]` ONLY** (`lib.rs`: `#[cfg(windows)] pub mod net_pcap;`). On Linux the NTP client is 100% rsntp. So a "mark the client request" change on Linux is impossible at the socket layer — it needs an nftables mangle rule at provisioning (`-p udp --dport 123 -j DSCP`).
- **Windows** filters a socket-set IP_TOS by default (`DisableUserTOSSetting`, since XP SP2), so DSCP on strih/stream needs a QoS policy (`New-NetQosPolicy` for UDP 123) at provisioning — NOT a socket call. `apply()` is deliberately a logged no-op on Windows (never a silent `setsockopt` that the OS discards).
- `time_server.rs` (port 31900, DSYN/DSYR) is a DIAGNOSTIC verification server, not timesync traffic — out of scope for marking.
- `run_supervised`'s rebind re-applies DSCP: `NtpServer` carries the `DscpConfig` and its `new()` calls `apply()`, so a restarted server re-marks its fresh socket.

## API

`socket2` is already a dep; `SockRef::from(&udp).set_tos(tos as u32)` marks a std
`UdpSocket` without consuming it. In socket2 **0.5.10** `set_tos` is NOT deprecated
(the `set_tos_v4` rename is later) — no clippy `-D warnings` risk. `apply()` is unit-
tested on unix by binding an ephemeral UDP socket and reading the TOS back with
`SockRef::tos()` (needs no privileges — only binding port 123 does).
