---
paths:
  - "src/ptp.rs"
  - "src/controller/ptp_sender.rs"
  - "src/controller/date_sync/gm_identity_tests.rs"
  - "src/date_offset/persist.rs"
  - "src/date_offset/wire.rs"
  - "tests/fixtures/ptpv1/**"
---

# The PTPv1 wire format and the grandmaster identity (dantesync#129)

## The layout, proven on real Dante bytes

IEEE 1588-2002 (PTPv1). A Dante Sync is 124 bytes, a Follow_Up 52. The header is **40** bytes.
The real packets are committed in `tests/fixtures/ptpv1/`, with their provenance.

| Absolute | Field |
|---|---|
| 0 / 2 | versionPTP u16 = 1 / versionNetwork u16 = 1 |
| 4..20 | subdomain, NUL-padded: `_DFLT` |
| 20 / 21 | messageType (1 event, 2 general) / sourceCommunicationTechnology |
| 22..28 / 28 / 30 | sourceUuid / sourcePortId u16 / sequenceId u16 |
| 32 | control (0 Sync, 2 Follow_Up) |
| 34 / 36..40 | flags / reserved |
| Sync 40..48 | originTimestamp (device uptime) |
| Sync 53 / 54..60 | grandmasterCommunicationTechnology / **grandmasterClockUuid** |
| Sync 60 / 62 | grandmasterPortId / grandmasterSequenceId |
| Sync 67 / 68..72 | grandmasterClockStratum / grandmasterClockIdentifier (`DFLT`) |
| Sync 74 / 77 / 79 | grandmasterClockVariance i16 / grandmasterPreferred / isBoundaryClock |
| Sync 83 | syncInterval i8 (−2 = 250 ms, the observed cadence: the layout's own cross-check) |
| Follow_Up 42 / 44..52 | associatedSequenceId / preciseOriginTimestamp |

**Before 1.17 the header was parsed as 36 bytes.**
- The Sync body was read from 36 with a 13-byte skip, so the GM UUID came from bytes 49..55 =
  `00 00 00 00 01 00` on every node: `ptp::LEGACY_MISREAD_GM_UUID`.
- The Follow_Up decode was right only by accident: 36 + 6 = 42 = 40 + 2. A change of the header
  size must change the Follow_Up skip with it. The real-bytes test pins the decoded seq + timestamp
  AND the absolute offsets.
- `versionPTP` was read as `byte0 >> 4` (0 on a real packet); `message_length` was versionNetwork.

## The legacy constant is still out there: three consumers handle it

The constant was published, saved and announced, so a 1.17 node meets it:

1. **The master's restart restore** (`DateOffsetState::validate_restore`). The saved UUID is
   report-only (`names_another_grandmaster` drives one info line; the pre-1.17 constant names
   none), and the time-base check decides. A 1.16 `date-offset.json` holds the constant: refusing
   it would boot-step the master to UTC on the upgrade's restart and move the fleet date. Review
   round 1 also caught that a 1.17 record of another port of the same clock would have been
   refused, a check that never refused before 1.17. `RestoreRejected::OtherGrandmaster` is gone.
2. **The follower's announce applicability** (`service_date_offset`). The GM UUID is report-only
   (debug log); `same_time_base` decides. Two ports of one clock (two VLANs) carry two UUIDs,
   and a 1.16 master announces the constant.
3. **The 31900 extension** (`date_extension_from_status`). A 1.17 node announces the constant, not
   its real anchor GM: a 1.16 follower adopts only an announce whose UUID equals its own misread
   anchor. So the upgrade order does not matter. The real UUID goes back on the wire in a later
   slice of issue 129, once no pre-1.17 node is left; 1.17+ followers never refuse on it.

Rollback of the MASTER below 1.17: delete `date-offset.json` (a 1.16 master refuses the real UUID).

## Report-only until the live identities are recorded

- `/status.ptp_version` / `ptp_subdomain` / `ptp_source_uuid` and the `PTP sender:` log line
  (`controller/ptp_sender.rs`, a child module so `controller.rs` does not grow) report the header.
- Nothing is dropped on them yet. Enforcing a version/subdomain/self filter waits for the table of
  every node's `gm_uuid` / `ptp_subdomain` / `ptp_source_uuid` on BOTH VLANs on issue 129.
- The GM-change detector works now: a real change calls `on_grandmaster_uuid_change` (re-anchor,
  no wall step). With one GM the GM UUID equals the Sync source UUID, so it fires together with the
  existing SYNC SOURCE CHANGED soft reset, never alone, unless a boundary clock relays a new GM.

## Capturing real packets (read-only)

On dev1 (passwordless sudo, rig video VLAN on `enp2s0`):
`sudo -n timeout 30 tcpdump -i enp2s0 -c 20 -w <scratch>.pcap 'udp port 319 or udp port 320'`.
No tshark on dev1. Extract the UDP payloads with a few lines of Python (pcap linktype 1:
Ethernet 14 + IPv4 IHL + UDP 8). Never restart or reconfigure anything for a capture.

## Local net for `src/ptp.rs` + `src/date_offset/` (Tier-0, no cargo)

A plain-rustc replica in about 10 s:
1. Build `byteorder` from `~/.cargo/registry/src/*/byteorder-1.5.0/src/lib.rs` with
   `--cfg 'feature="std"'`.
2. Stub `anyhow`: an `Error(String)` with a blanket `From<E: std::error::Error>`, `Result<T>`,
   and an `anyhow!` macro.
3. Make a scratch crate whose `src/` symlinks `ptp.rs`, `date_offset.rs` and `date_offset/`, plus
   a `tests` symlink beside `src` (the `include_bytes!("../tests/fixtures/…")` resolves relative to
   the symlink's place).
4. Run `rustc --edition 2021 --test src/lib.rs --crate-name dantesync --extern …` (131 tests at
   1.17). The same lib under `CARGO_PKG_RUST_VERSION=1.70.0 clippy-driver … -D warnings -A
   dead_code` is the MSRV lint.

The controller, `time_server` and `status` tests stay CI-only. They need the #126 full-lib
replica in `clock-discipline-and-testing.md`.
