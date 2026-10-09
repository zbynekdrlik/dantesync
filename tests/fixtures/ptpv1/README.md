# Real Dante PTPv1 packets (issue 129)

Two UDP payloads, byte for byte as they arrived:

- `dante-sync.bin` (124 bytes): a Sync on 224.0.1.129:319.
- `dante-follow-up.bin` (52 bytes): its Follow_Up on 224.0.1.129:320, the same sequence id 0xb27a.

## Provenance

- **Box:** dev1 (`10.77.9.109/23`), interface `enp2s0`, the SNV rig's video VLAN.
- **Time:** 2026-10-09T21:17:17Z.
- **Sender:** the Dante grandmaster `10.77.9.230`, source UUID `00:1d:c1:08:02:14` (Audinate OUI).
- **Capture:** read-only, nothing restarted or reconfigured:
  `sudo tcpdump -i enp2s0 -c 20 -w dev1-ptpv1.pcap 'udp port 319 or udp port 320'`.
  The pcap held 10 Sync + 10 Follow_Up, every 250 ms (`syncInterval` −2). Packets 0 and 1 were
  extracted by stripping the Ethernet, IPv4 and UDP headers.

## Decoded (IEEE 1588-2002)

**Header (40 bytes).**
- versionPTP 1, versionNetwork 1, subdomain `_DFLT`.
- messageType 1 (event) / 2 (general), sourceCommunicationTechnology 1.
- sourceUuid `00:1d:c1:08:02:14`, sourcePortId 2, sequenceId 0xb27a.
- control 0 (Sync) / 2 (Follow_Up), flags 0x000c.

**Sync body (84 bytes):**
- originTimestamp 541867 s + 434210408 ns (device uptime);
- grandmasterCommunicationTechnology 1, grandmasterClockUuid `00:1d:c1:08:02:14`, grandmasterPortId 0,
  grandmasterSequenceId 0xb27a;
- grandmasterClockStratum 0x79, grandmasterClockIdentifier `DFLT`, grandmasterClockVariance −4000,
  grandmasterPreferred 1, grandmasterIsBoundaryClock 1;
- syncInterval −2, localClockVariance −4000, localStepsRemoved 0, localClockStratum 0x79,
  localClockIdentifier `DFLT`;
- parentUuid `00:1d:c1:08:02:14`, estimatedMasterVariance −7.

**Follow_Up body (12 bytes):** associatedSequenceId 0xb27a, preciseOriginTimestamp 541867 s +
434557859 ns.

Absolute bytes 49..55 of the Sync are `00 00 00 00 01 00`: what every node before 1.17 read as the
grandmaster UUID (a 36-byte header plus a 13-byte skip).
