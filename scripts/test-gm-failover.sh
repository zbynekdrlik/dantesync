#!/bin/bash
# test-gm-failover.sh - Grandmaster failover recovery test
#
# MANUAL STEPS REQUIRED:
# 1. Start this script
# 2. When prompted, power off the primary Dante grandmaster
# 3. Wait for a secondary device to take over GM role
# 4. Observe sync behavior
# 5. Power primary GM back on
# 6. Script will analyze recovery

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT="gm_failover_test_$(date +%Y%m%d_%H%M%S).csv"

echo "=== Grandmaster Failover Test ==="
echo ""
echo "This test verifies DanteSync handles GM changes correctly."
echo "Output: $OUTPUT"
echo ""

# Start monitoring
"$SCRIPT_DIR/sync-monitor.sh" 5 "$OUTPUT" &
MONITOR_PID=$!

trap "kill $MONITOR_PID 2>/dev/null; exit 1" INT TERM

echo "Monitoring started (PID: $MONITOR_PID)"
echo "Collecting baseline for 60 seconds..."
sleep 60

echo ""
echo "========================================================"
echo "  ACTION REQUIRED: Power OFF the primary Dante Grandmaster"
echo "========================================================"
read -p "Press ENTER when GM is powered off..."

FAILOVER_TIME=$(date +%s.%N)
echo "GM powered off at: $(date)"
echo ""
echo "Monitoring sync behavior during failover..."
echo "Observing for 120 seconds..."
sleep 120

echo ""
echo "========================================================"
echo "  ACTION REQUIRED: Power ON the primary Dante Grandmaster"
echo "========================================================"
read -p "Press ENTER when GM is powered back on..."

RECOVERY_TIME=$(date +%s.%N)
echo "GM powered on at: $(date)"
echo ""
echo "Monitoring recovery for 120 seconds..."
sleep 120

# Stop monitoring
kill $MONITOR_PID 2>/dev/null || true
wait $MONITOR_PID 2>/dev/null || true

echo ""
echo "=== Failover Test Analysis ==="
echo ""
python3 "$SCRIPT_DIR/analyze-sync.py" "$OUTPUT"

echo ""
echo "=== Expected Behavior ==="
echo "1. During failover: Temporary increase in spread (new GM has different frequency)"
echo "2. DanteSync should detect new GM and re-acquire lock within ~30s"
echo "3. After recovery: Spread should return to baseline"
echo "4. No computer should show sustained >10ms offset"
echo ""
echo "Failover time: $FAILOVER_TIME"
echo "Recovery time: $RECOVERY_TIME"
echo ""
echo "Review $OUTPUT for detailed timestamps."
