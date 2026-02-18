---
name: sync-monitoring
description: Monitor and verify DanteSync clock synchronization across multiple computers. Use when checking sync status, running comparison tests, or verifying NTP/PTP alignment.
---

# DanteSync Multi-Computer Sync Monitoring

This skill provides tools to verify clock synchronization across the DanteSync network.

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

## Quick Status Check (Recommended)

The preferred method is to use the UDP Time Query API via the sync-snapshot.py script.
This queries all targets in parallel with ~1ms latency (vs 100-500ms SSH latency).

```bash
# Quick snapshot of all targets
python3 scripts/sync-snapshot.py

# Query specific hosts only
python3 scripts/sync-snapshot.py --hosts strih.lan develbox stream.lan

# Use a different reference host
python3 scripts/sync-snapshot.py --reference develbox
```

The UDP Time Query API (port 31900) returns:

- System time (UTC nanoseconds)
- Monotonic counter (hardware tick count)
- PTP offset from grandmaster
- Sync mode (ACQ, PROD, LOCK, NANO)
- Is locked status

## Available Scripts

All scripts are in the `scripts/` directory:

| Script                   | Purpose                      | Usage                                               |
| ------------------------ | ---------------------------- | --------------------------------------------------- |
| `sync-snapshot.py`       | **Parallel UDP time query**  | `python3 scripts/sync-snapshot.py`                  |
| `sync-monitor.sh`        | Long-term CSV monitoring     | `./scripts/sync-monitor.sh [interval] [output.csv]` |
| `compare-all-targets.sh` | Single-moment comparison     | `./scripts/compare-all-targets.sh`                  |
| `sync-dashboard.sh`      | Real-time terminal dashboard | `./scripts/sync-dashboard.sh`                       |
| `analyze-sync.py`        | Analyze monitoring CSV       | `python3 scripts/analyze-sync.py <csv_file>`        |

**Note:** The `sync-snapshot.py` script is the recommended tool for quick verification.
It uses UDP queries which are ~100x faster than SSH-based methods.

## UDP Time Query Protocol

DanteSync includes a UDP Time Query server on port 31900 that returns precise clock data:

**Request:** 8 bytes (`DSYN` magic + request_id)
**Response:** 64 bytes containing:

- System time (UTC nanoseconds since Unix epoch)
- Monotonic counter (QPC on Windows, CLOCK_MONOTONIC_RAW on Linux)
- PTP offset from grandmaster (nanoseconds)
- Drift rate (PPM)
- Frequency adjustment (PPM)
- Mode (INIT/ACQ/PROD/LOCK/NANO/NTP-only)
- Is locked flag
- Grandmaster UUID
- Monotonic frequency (for normalization)

## Interpreting Results

**Sync Quality Thresholds (from sync-snapshot.py):**

- `< 1ms`: SYNCED - All clocks aligned
- `1-5ms`: DRIFT - May need investigation
- `> 5ms`: ERROR - Sync problem detected

**Sync Modes:**

- `ACQ` - Acquiring lock (fast convergence)
- `PROD` - Production mode (stable)
- `LOCK` - Locked (frequency stable < 5us/s)
- `NANO` - Ultra-precise mode (< 0.5us/s)
- `NTP-only` - No PTP, NTP fallback

## Troubleshooting

**If sync-snapshot.py shows OFFLINE:**

1. Check if dantesync service is running on target
2. Check if UDP port 31900 is open in firewall
3. Verify network connectivity

**If a client shows large offset:**

1. Check if dantesync service is running: `sc query dantesync` (Windows) or `systemctl status dantesync` (Linux)
2. Check if pointing to correct NTP server in config
3. Verify strih.lan is PTP-locked (check for "LOCK" mode)

**If NTP master (strih.lan) loses PTP lock:**

1. Check Dante network connectivity
2. Verify grandmaster device is powered on
3. Check for "ACQ" or "PROD" mode (still acquiring)

## Config Locations

- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`

NTP server should be `10.77.9.202` (strih.lan) for all clients.
