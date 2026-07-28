---
name: sync-monitoring
description: Monitor and verify DanteSync clock synchronization across multiple computers. Use when checking sync status, running comparison tests, or verifying NTP/PTP alignment.
---

# DanteSync Sync Monitoring

## Quick Status Check

```bash
python3 scripts/sync-snapshot.py           # Full comprehensive report
python3 scripts/sync-snapshot.py --brief   # Quick status summary
python3 scripts/sync-snapshot.py --json    # JSON output for scripting
python3 scripts/sync-snapshot.py --hosts strih.lan develbox  # Specific hosts only
```

## Network Architecture

```
strih.lan (10.77.9.202) - NTP MASTER
    └── PTP locked to Dante grandmaster
    └── Serves NTP to all clients

Clients (sync to strih.lan via NTP):
    - develbox (10.77.9.21) - Linux
    - stream.lan (10.77.9.204) - Windows
    - iem (10.77.9.231) - Windows
    - ableton-foh (10.77.9.230) - Windows
    - mbc.lan (10.77.9.232) - Windows
    - songs (10.77.9.212) - Windows
    - stagebox1.lan (10.77.9.237) - Windows
    - piano.lan (10.77.9.236) - Windows
```

## Interpreting Results

**Sync Quality Thresholds:**

- `< 1ms`: SYNCED - All clocks aligned
- `1-5ms`: DRIFT - May need investigation
- `> 5ms`: ERROR - Sync problem detected

**NTP measurement quality (dantesync#53):** `ntp_offset_us` (status JSON / tray) is now a
burst-filtered, RTT-selected MEDIAN (5 raw samples/check, lowest-3-RTT accepted), not a single raw
round trip — a single loaded-Windows-box round trip used to scatter ±20ms even while PTP reported
locked. Two new fields expose whether to TRUST that median: `ntp_spread_us` (max-min across the
accepted samples — large even when the offset itself looks reasonable means the node's NTP
measurement is still noisy) and `ntp_sample_count` (how many samples backed it — lower than 3 means
a partial burst failure). **Never read `ntp_offset_us` alone as "this node is fine" — check
`ntp_spread_us` too.** A wide spread on an otherwise-locked node points at userspace scheduling
jitter on THAT box (CPU load, thread starvation), not a network/PTP problem.

**Sync Modes:**

- `ACQ` - Acquiring lock (fast convergence)
- `PROD` - Production mode (stable)
- `LOCK` - Locked (frequency stable < 5us/s)
- `NANO` - Ultra-precise mode (< 0.5us/s)
- `NTP-only` - No PTP, NTP fallback

## Troubleshooting

**OFFLINE host:** Check service running + UDP port 31900 open in firewall

**Large offset:** Check NTP config points to `10.77.9.202` (strih.lan)

**Config Locations:**

- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`
