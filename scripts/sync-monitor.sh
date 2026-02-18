#!/bin/bash
# sync-monitor.sh - Long-term DanteSync monitoring
# Usage: ./sync-monitor.sh [interval_seconds] [output_file]
#
# Continuously collects timestamps from all targets and logs differences.
# Use with analyze-sync.py to generate reports.

set -e

INTERVAL=${1:-10}
OUTPUT=${2:-sync_monitor_$(date +%Y%m%d_%H%M%S).csv}

# Target definitions (from TARGETS.md, excluding offline targets)
# Format: hostname:user@ip:os
declare -a TARGETS=(
    "develbox:newlevel@10.77.9.21:linux"
    "strih.lan:newlevel@10.77.9.202:win"
    "stream.lan:newlevel@10.77.9.204:win"
    "iem:iem@10.77.9.231:win"
    "ableton-foh:ableton-foh@10.77.9.230:win"
    "mbc.lan:newlevel@10.77.9.232:win"
    "songs:newlevel@10.77.9.212:win"
    "stagebox1.lan:newlevel@10.77.9.237:win"
    "piano.lan:alexnb@10.77.9.236:win"
)

# Reference host (Linux for nanosecond precision)
REF_HOST="develbox"

# Build CSV header
HEADER="timestamp,reference_host"
for target in "${TARGETS[@]}"; do
    name=${target%%:*}
    HEADER="$HEADER,$name"
done
HEADER="$HEADER,max_diff_ms,min_diff_ms,spread_ms"
echo "$HEADER" > "$OUTPUT"

echo "=== DanteSync Long-Term Monitor ==="
echo "Interval: ${INTERVAL}s"
echo "Output: $OUTPUT"
echo "Reference: $REF_HOST"
echo "Targets: ${#TARGETS[@]} hosts"
echo "Press Ctrl+C to stop"
echo ""

# Function to get time from a host
get_time_ns() {
    local user_host=$1
    local os=$2

    if [[ "$os" == "linux" ]]; then
        # Linux: nanosecond precision
        ssh -o ConnectTimeout=2 -o StrictHostKeyChecking=no "$user_host" "date +%s.%N" 2>/dev/null
    else
        # Windows: PowerShell with millisecond precision
        ssh -o ConnectTimeout=2 -o StrictHostKeyChecking=no "$user_host" \
            "powershell -Command \"[Math]::Round((Get-Date).ToUniversalTime().Subtract([DateTime]'1970-01-01').TotalSeconds, 6)\"" 2>/dev/null
    fi
}

# Main monitoring loop
while true; do
    NOW=$(date +%s.%N)

    # Get reference time first
    REF_TIME=""
    for target in "${TARGETS[@]}"; do
        name=${target%%:*}
        if [[ "$name" == "$REF_HOST" ]]; then
            rest=${target#*:}
            user_host=${rest%%:*}
            os=${rest##*:}
            REF_TIME=$(get_time_ns "$user_host" "$os")
            break
        fi
    done

    if [[ -z "$REF_TIME" ]]; then
        echo "[$(date +%H:%M:%S)] Reference host $REF_HOST unreachable, skipping..."
        sleep "$INTERVAL"
        continue
    fi

    # Collect all timestamps and calculate differences
    CSV_LINE="$NOW,$REF_HOST"
    MAX_DIFF=0
    MIN_DIFF=999999
    VALID_COUNT=0

    for target in "${TARGETS[@]}"; do
        name=${target%%:*}
        rest=${target#*:}
        user_host=${rest%%:*}
        os=${rest##*:}

        if [[ "$name" == "$REF_HOST" ]]; then
            CSV_LINE="$CSV_LINE,0.000"
            continue
        fi

        TIME=$(get_time_ns "$user_host" "$os")

        if [[ -n "$TIME" ]]; then
            # Calculate difference in milliseconds
            DIFF=$(echo "scale=6; ($TIME - $REF_TIME) * 1000" | bc)
            CSV_LINE="$CSV_LINE,$DIFF"

            ABS_DIFF=$(echo "${DIFF#-}" | bc)
            if (( $(echo "$ABS_DIFF > $MAX_DIFF" | bc -l) )); then
                MAX_DIFF=$ABS_DIFF
            fi
            if (( $(echo "$ABS_DIFF < $MIN_DIFF" | bc -l) )); then
                MIN_DIFF=$ABS_DIFF
            fi
            VALID_COUNT=$((VALID_COUNT + 1))
        else
            CSV_LINE="$CSV_LINE,N/A"
        fi
    done

    # Handle case where no valid samples
    if [[ $VALID_COUNT -eq 0 ]]; then
        MIN_DIFF=0
    fi

    SPREAD=$(echo "scale=3; $MAX_DIFF - $MIN_DIFF" | bc)
    CSV_LINE="$CSV_LINE,$MAX_DIFF,$MIN_DIFF,$SPREAD"

    # Log to CSV
    echo "$CSV_LINE" >> "$OUTPUT"

    # Console output
    printf "[%s] Max: %.3fms  Spread: %.3fms  (%d hosts)\n" \
        "$(date +%H:%M:%S)" "$MAX_DIFF" "$SPREAD" "$VALID_COUNT"

    sleep "$INTERVAL"
done
