---
name: sync-dashboard
description: Launch real-time sync status dashboard in terminal
---

Launch the real-time sync status dashboard showing all targets.

## Instructions

1. Run the dashboard script:

   ```bash
   ./scripts/sync-dashboard.sh
   ```

2. The dashboard shows:
   - Current time
   - Each host's offset from reference (develbox)
   - Sync status: LOCKED / SYNCED / DRIFT / ERROR / OFFLINE
   - Last update time

3. Dashboard refreshes every 5 seconds.

4. Press Ctrl+C to exit.

5. Note: This provides a quick visual overview. For detailed analysis, use `/sync-monitor` to collect data over time.
