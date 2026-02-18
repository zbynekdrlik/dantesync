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
```

## Quick Status Check

To check current sync status across all targets:

```bash
# Check NTP master (strih.lan) PTP lock status
ssh newlevel@10.77.9.202 "powershell -Command \"Get-Content 'C:\\ProgramData\\DanteSync\\dantesync.log' -Tail 5\""

# Check each client's NTP offset from logs
for host in "newlevel@10.77.9.21" "newlevel@10.77.9.204" "iem@10.77.9.231" "ableton-foh@10.77.9.230"; do
    echo "=== $host ==="
    if [[ "$host" == *"10.77.9.21"* ]]; then
        ssh "$host" "tail -5 /var/log/dantesync/dantesync.log 2>/dev/null | grep -E 'offset|NTP'" || echo "Log unavailable"
    else
        ssh "$host" "powershell -Command \"Get-Content 'C:\\ProgramData\\DanteSync\\dantesync.log' -Tail 5 | Select-String -Pattern 'offset|NTP'\"" 2>/dev/null || echo "Log unavailable"
    fi
done
```

## Available Scripts

All scripts are in the `scripts/` directory:

| Script                    | Purpose                      | Usage                                                  |
| ------------------------- | ---------------------------- | ------------------------------------------------------ |
| `sync-monitor.sh`         | Long-term CSV monitoring     | `./scripts/sync-monitor.sh [interval] [output.csv]`    |
| `compare-all-targets.sh`  | Single-moment comparison     | `./scripts/compare-all-targets.sh`                     |
| `sync-dashboard.sh`       | Real-time terminal dashboard | `./scripts/sync-dashboard.sh`                          |
| `test-long-running.sh`    | 4-hour stability test        | `./scripts/test-long-running.sh`                       |
| `test-gm-failover.sh`     | Grandmaster failover test    | `./scripts/test-gm-failover.sh`                        |
| `test-reboot-recovery.sh` | Reboot recovery test         | `./scripts/test-reboot-recovery.sh [host] [user] [ip]` |
| `analyze-sync.py`         | Analyze monitoring CSV       | `python3 scripts/analyze-sync.py <csv_file>`           |

## Interpreting Results

**Sync Quality Thresholds:**

- `< 1ms`: EXCELLENT - Sub-millisecond sync
- `1-5ms`: GOOD - Acceptable for most applications
- `5-10ms`: DRIFT - May need investigation
- `> 10ms`: ERROR - Sync problem detected

**NTP Log Offset Values:**

- `< 500µs`: Optimal - tightly synced
- `500µs - 2ms`: Normal operating range
- `> 5ms`: Investigate - check PTP lock on master

## Troubleshooting

**If a client shows large offset:**

1. Check if dantesync service is running: `sc query dantesync` (Windows) or `systemctl status dantesync` (Linux)
2. Check if pointing to correct NTP server in config
3. Verify strih.lan is PTP-locked (check for "LOCK" in its log)

**If NTP master (strih.lan) loses PTP lock:**

1. Check Dante network connectivity
2. Verify grandmaster device is powered on
3. Check for "ACQ" or "PROD" mode in log (still acquiring)

## Config Locations

- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`

NTP server should be `10.77.9.202` (strih.lan) for all clients.
