---
name: sync-compare
description: Run a quick timestamp comparison across all DanteSync targets
---

Run a quick timestamp comparison across all online DanteSync targets using the UDP Time Query API.

## Instructions

1. Run the sync-snapshot.py script to query all targets in parallel:

   ```bash
   python3 scripts/sync-snapshot.py
   ```

2. The script queries UDP port 31900 on each target simultaneously (~1ms latency).

3. Interpret the results:
   - SYNCED: < 1ms difference (excellent)
   - DRIFT: 1-5ms difference (investigate)
   - ERROR: > 5ms difference (problem)
   - OFFLINE: Host unreachable (check firewall/service)

4. Optional arguments:

   ```bash
   # Query specific hosts only
   python3 scripts/sync-snapshot.py --hosts strih.lan develbox

   # Use different reference host
   python3 scripts/sync-snapshot.py --reference develbox

   # Increase timeout for slow networks
   python3 scripts/sync-snapshot.py --timeout 1.0
   ```

5. The output shows:
   - System time offset (NTP-corrected wall clock)
   - Monotonic counter offset (true tick alignment)
   - Sync mode and lock status per host
   - Grandmaster UUID (should match across all hosts)

6. If UDP port 31900 is blocked, ensure firewall allows it on all targets.

## Legacy Scripts

The older SSH-based scripts are still available but have 100-500ms latency:

- `./scripts/compare-all-targets.sh` - SSH-based comparison (deprecated)
