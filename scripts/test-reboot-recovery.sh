#!/bin/bash
# test-reboot-recovery.sh - Reboot recovery test
#
# Tests that a rebooted computer correctly re-syncs with the network.
# Usage: ./test-reboot-recovery.sh [hostname] [user] [ip]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Default target (can be overridden)
TEST_HOST=${1:-"strih.lan"}
TEST_USER=${2:-"newlevel"}
TEST_IP=${3:-"10.77.9.202"}
OUTPUT="reboot_recovery_test_$(date +%Y%m%d_%H%M%S).csv"

echo "=== Reboot Recovery Test ==="
echo "Target: $TEST_HOST ($TEST_IP)"
echo "Output: $OUTPUT"
echo ""

# Start monitoring
"$SCRIPT_DIR/sync-monitor.sh" 5 "$OUTPUT" &
MONITOR_PID=$!

trap "kill $MONITOR_PID 2>/dev/null; exit 1" INT TERM

echo "Monitoring started (PID: $MONITOR_PID)"
echo "Collecting baseline for 60 seconds..."
sleep 60

# Calculate baseline offset for test host
echo "Calculating baseline offset for $TEST_HOST..."
BASELINE=$(tail -10 "$OUTPUT" | awk -F',' -v host="$TEST_HOST" '
    BEGIN { sum=0; count=0 }
    NR==1 {
        for(i=1; i<=NF; i++) {
            if($i == host) { col=i; break }
        }
        next
    }
    {
        if(col && $col != "N/A" && $col != "") {
            sum += $col
            count++
        }
    }
    END {
        if(count > 0) print sum/count
        else print "N/A"
    }
')
echo "Baseline offset for $TEST_HOST: ${BASELINE}ms"

echo ""
echo "Initiating reboot of $TEST_HOST..."

# Try Windows reboot first, then Linux
if ssh -o ConnectTimeout=5 "$TEST_USER@$TEST_IP" "shutdown /r /t 5 /f" 2>/dev/null; then
    echo "Windows reboot command sent"
elif ssh -o ConnectTimeout=5 "$TEST_USER@$TEST_IP" "sudo reboot" 2>/dev/null; then
    echo "Linux reboot command sent"
else
    echo "WARNING: Could not send reboot command. You may need to reboot manually."
    read -p "Press ENTER when $TEST_HOST has been rebooted..."
fi

REBOOT_TIME=$(date +%s.%N)
echo "Reboot initiated at: $(date)"

echo ""
echo "Waiting for reboot and DanteSync startup (180 seconds)..."
sleep 180

echo "Collecting post-reboot data for 120 seconds..."
sleep 120

# Stop monitoring
kill $MONITOR_PID 2>/dev/null || true
wait $MONITOR_PID 2>/dev/null || true

echo ""
echo "=== Reboot Recovery Analysis ==="
python3 "$SCRIPT_DIR/analyze-sync.py" "$OUTPUT"

# Calculate post-reboot offset
POST_OFFSET=$(tail -10 "$OUTPUT" | awk -F',' -v host="$TEST_HOST" '
    BEGIN { sum=0; count=0 }
    NR==1 {
        for(i=1; i<=NF; i++) {
            if($i == host) { col=i; break }
        }
        next
    }
    {
        if(col && $col != "N/A" && $col != "") {
            sum += $col
            count++
        }
    }
    END {
        if(count > 0) print sum/count
        else print "N/A"
    }
')

echo ""
echo "=== RECOVERY METRICS ==="
echo "Pre-reboot offset:  ${BASELINE}ms"
echo "Post-reboot offset: ${POST_OFFSET}ms"

if [[ "$BASELINE" != "N/A" && "$POST_OFFSET" != "N/A" ]] && command -v bc &> /dev/null; then
    DIFF=$(echo "scale=3; ($POST_OFFSET) - ($BASELINE)" | bc)
    DIFF_ABS=${DIFF#-}

    if (( $(echo "$DIFF_ABS < 5" | bc -l) )); then
        echo "PASS: Post-reboot offset within 5ms of baseline"
        exit 0
    else
        echo "FAIL: Post-reboot offset differs by ${DIFF_ABS}ms from baseline"
        exit 1
    fi
else
    echo "Note: Manual review required (baseline or post-reboot data incomplete)"
fi
