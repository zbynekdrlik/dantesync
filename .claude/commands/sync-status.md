---
name: sync-status
description: Check current DanteSync synchronization status across all network targets
---

Check the current clock synchronization status across all DanteSync targets using the UDP Time Query API.

## Instructions

1. Run the sync-snapshot.py script to get instant status:

   ```bash
   python3 scripts/sync-snapshot.py
   ```

2. The output shows for each host:
   - System time (wall clock)
   - Sync mode (ACQ/PROD/LOCK/NANO)
   - Lock status
   - Round-trip time

3. The offset analysis shows:
   - System time difference (should be < 1ms)
   - Monotonic counter difference (true tick alignment)
   - Grandmaster UUID (should match across hosts)

4. Flag any issues:
   - Host showing OFFLINE - check firewall/service
   - Mode not LOCK or NANO - still acquiring
   - Offset > 1ms - sync problem
   - Different GM UUIDs - network segmentation

## Network Targets

- strih.lan (10.77.9.202) - NTP MASTER
- develbox (10.77.9.21) - Linux
- stream.lan (10.77.9.204) - Windows
- iem (10.77.9.231) - Windows
- ableton-foh (10.77.9.230) - Windows
- mbc.lan (10.77.9.232) - Windows
- songs (10.77.9.212) - Windows
- stagebox1.lan (10.77.9.237) - Windows
- piano.lan (10.77.9.236) - Windows

## Troubleshooting

If UDP queries fail, fall back to SSH log checks:

```bash
# Check Windows host log
ssh newlevel@10.77.9.202 "powershell -Command \"Get-Content 'C:\\ProgramData\\DanteSync\\dantesync.log' -Tail 10\""

# Check Linux host log
ssh newlevel@10.77.9.21 "tail -10 /var/log/dantesync/dantesync.log"
```
