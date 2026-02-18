---
name: sync-status
description: Check current DanteSync synchronization status across all network targets
---

Check the current clock synchronization status across all DanteSync targets.

## Instructions

1. Check the NTP master (strih.lan / 10.77.9.202) PTP lock status by reading its dantesync.log
2. Check each client's NTP offset from their logs:
   - develbox (10.77.9.21) - Linux
   - stream.lan (10.77.9.204) - Windows
   - iem (10.77.9.231) - Windows
   - ableton-foh (10.77.9.230) - Windows

3. Report the status in a table format:
   | Host | Role | Status | Offset | Notes |

4. Flag any issues:
   - Master not in LOCK mode
   - Client offset > 5ms
   - Service not running
   - Host unreachable

Use SSH to query the logs. For Windows hosts use PowerShell commands via SSH.
