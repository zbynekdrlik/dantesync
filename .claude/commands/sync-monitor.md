---
name: sync-monitor
description: Start long-term sync monitoring with CSV output
---

Start long-term synchronization monitoring across all DanteSync targets.

## Instructions

1. Run the sync-monitor.sh script:

   ```bash
   ./scripts/sync-monitor.sh [interval_seconds] [output_file.csv]
   ```

   Default interval is 10 seconds if not specified.
   Default output file is `sync_monitor_YYYYMMDD_HHMMSS.csv`

2. The script will continuously collect timestamps from all targets and log:
   - Timestamp
   - Reference host
   - Offset for each target (in milliseconds)
   - Max/min difference and spread

3. To analyze collected data:

   ```bash
   python3 scripts/analyze-sync.py <output_file.csv>
   ```

4. Press Ctrl+C to stop monitoring.

5. Success criteria:
   - Average spread < 5ms
   - No outliers > 20ms
   - All targets should remain online
