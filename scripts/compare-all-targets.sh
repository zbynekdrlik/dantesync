#!/bin/bash
# compare-all-targets.sh - Single-moment comparison of all targets
#
# Quick snapshot comparing timestamps across all targets.
# Useful for spot-checking sync state.

set -e

echo "=== DanteSync All-Targets Comparison ==="
echo "Timestamp: $(date)"
echo ""

# Target definitions (from TARGETS.md)
declare -A TARGETS=(
    ["develbox"]="newlevel@10.77.9.21:linux"
    ["strih.lan"]="newlevel@10.77.9.202:win"
    ["stream.lan"]="newlevel@10.77.9.204:win"
    ["iem"]="iem@10.77.9.231:win"
    ["ableton-foh"]="ableton-foh@10.77.9.230:win"
    ["mbc.lan"]="newlevel@10.77.9.232:win"
    ["songs"]="newlevel@10.77.9.212:win"
    ["stagebox1.lan"]="newlevel@10.77.9.237:win"
    ["piano.lan"]="alexnb@10.77.9.236:win"
)

# Reference host
REF_HOST="develbox"

get_time() {
    local user_host=$1
    local os=$2

    if [[ "$os" == "linux" ]]; then
        ssh -o ConnectTimeout=3 -o StrictHostKeyChecking=no "$user_host" "date +%s.%N" 2>/dev/null
    else
        ssh -o ConnectTimeout=3 -o StrictHostKeyChecking=no "$user_host" \
            "powershell -Command \"[Math]::Round((Get-Date).ToUniversalTime().Subtract([DateTime]'1970-01-01').TotalSeconds, 6)\"" 2>/dev/null
    fi
}

# Get reference time
REF_INFO=${TARGETS[$REF_HOST]}
REF_USER_HOST=${REF_INFO%%:*}
REF_OS=${REF_INFO##*:}
REF_TIME=$(get_time "$REF_USER_HOST" "$REF_OS")

if [[ -z "$REF_TIME" ]]; then
    echo "ERROR: Reference host $REF_HOST unreachable"
    exit 1
fi

echo "Reference: $REF_HOST = $REF_TIME"
echo ""
printf "%-15s | %20s | %12s | %s\n" "Host" "Timestamp" "Diff (ms)" "Status"
printf "%s\n" "---------------------------------------------------------------"

MAX_DIFF=0
ALL_OK=true

for host in "${!TARGETS[@]}"; do
    info=${TARGETS[$host]}
    user_host=${info%%:*}
    os=${info##*:}

    TIME=$(get_time "$user_host" "$os")

    if [[ -n "$TIME" ]]; then
        DIFF=$(echo "scale=3; ($TIME - $REF_TIME) * 1000" | bc)
        ABS_DIFF=${DIFF#-}

        if (( $(echo "$ABS_DIFF < 1" | bc -l) )); then
            STATUS="LOCKED"
        elif (( $(echo "$ABS_DIFF < 5" | bc -l) )); then
            STATUS="SYNCED"
        elif (( $(echo "$ABS_DIFF < 10" | bc -l) )); then
            STATUS="DRIFT"
        else
            STATUS="ERROR"
            ALL_OK=false
        fi

        printf "%-15s | %20s | %+11.3f | %s\n" "$host" "$TIME" "$DIFF" "$STATUS"

        if (( $(echo "$ABS_DIFF > $MAX_DIFF" | bc -l) )); then
            MAX_DIFF=$ABS_DIFF
        fi
    else
        printf "%-15s | %20s | %12s | %s\n" "$host" "N/A" "N/A" "OFFLINE"
        ALL_OK=false
    fi
done

echo ""
echo "=== SUMMARY ==="
echo "Max difference: ${MAX_DIFF}ms"

if $ALL_OK; then
    echo "Status: All targets within tolerance"
    exit 0
else
    echo "Status: Some targets have issues (see above)"
    exit 1
fi
