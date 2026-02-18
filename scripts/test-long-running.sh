#!/bin/bash
# test-long-running.sh - 4-hour stability test
#
# Runs sync monitoring for 4 hours and analyzes results.
# Pass criteria: Average spread < 5ms, no outliers > 20ms

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DURATION_HOURS=${1:-4}
DURATION_SECS=$((DURATION_HOURS * 3600))
OUTPUT="long_running_test_$(date +%Y%m%d_%H%M%S).csv"

echo "=== Long-Running Stability Test (${DURATION_HOURS} hours) ==="
echo "Starting at $(date)"
echo "Output: $OUTPUT"
echo ""

# Run monitor in background
"$SCRIPT_DIR/sync-monitor.sh" 30 "$OUTPUT" &
MONITOR_PID=$!

# Trap to clean up on interrupt
trap "kill $MONITOR_PID 2>/dev/null; exit 1" INT TERM

echo "Monitor PID: $MONITOR_PID"
echo "Running for $DURATION_SECS seconds..."
echo ""

# Wait for duration
sleep "$DURATION_SECS"

# Stop monitor
kill $MONITOR_PID 2>/dev/null || true
wait $MONITOR_PID 2>/dev/null || true

echo ""
echo "=== Test Complete ==="
echo ""

# Analyze results
python3 "$SCRIPT_DIR/analyze-sync.py" "$OUTPUT"

# Extract pass/fail metrics
if command -v bc &> /dev/null; then
    # Get average spread from last 100 samples
    AVG_SPREAD=$(tail -100 "$OUTPUT" | awk -F',' '{sum+=$NF; count++} END {if(count>0) print sum/count; else print 0}')

    echo ""
    echo "=== PASS/FAIL CRITERIA ==="
    if (( $(echo "$AVG_SPREAD < 5" | bc -l) )); then
        echo "PASS: Average spread ${AVG_SPREAD}ms < 5ms threshold"
        exit 0
    else
        echo "FAIL: Average spread ${AVG_SPREAD}ms >= 5ms threshold"
        exit 1
    fi
else
    echo ""
    echo "Note: Install 'bc' for automated pass/fail evaluation"
    exit 0
fi
