---
name: sync-compare
description: Run a quick timestamp comparison across all DanteSync targets
---

Run a quick timestamp comparison across all online DanteSync targets.

## Instructions

1. Run the compare-all-targets.sh script from the scripts directory:

   ```bash
   ./scripts/compare-all-targets.sh
   ```

2. If the script is not executable, make it executable first:

   ```bash
   chmod +x ./scripts/compare-all-targets.sh
   ```

3. Interpret the results:
   - LOCKED: < 1ms difference (excellent)
   - SYNCED: < 5ms difference (good)
   - DRIFT: < 10ms difference (investigate)
   - ERROR: > 10ms difference (problem)
   - OFFLINE: Host unreachable

4. Note: SSH round-trip latency adds ~100-200ms apparent offset. For true sync quality, check the NTP offset values in each host's dantesync.log.
