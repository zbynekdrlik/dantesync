#!/bin/bash
# sync-dashboard.sh - Real-time sync status dashboard
#
# Displays a continuously updating terminal dashboard showing
# sync status of all targets.

set -e

# Target definitions (from TARGETS.md) - subset for dashboard
declare -A TARGETS=(
    ["develbox"]="newlevel@10.77.9.21:linux"
    ["strih.lan"]="newlevel@10.77.9.202:win"
    ["stream.lan"]="newlevel@10.77.9.204:win"
    ["iem"]="iem@10.77.9.231:win"
    ["ableton-foh"]="ableton-foh@10.77.9.230:win"
)

REF_HOST="develbox"
REFRESH_INTERVAL=5

get_time() {
    local user_host=$1
    local os=$2

    if [[ "$os" == "linux" ]]; then
        ssh -o ConnectTimeout=2 -o StrictHostKeyChecking=no "$user_host" "date +%s.%N" 2>/dev/null
    else
        ssh -o ConnectTimeout=2 -o StrictHostKeyChecking=no "$user_host" \
            "powershell -Command \"[Math]::Round((Get-Date).ToUniversalTime().Subtract([DateTime]'1970-01-01').TotalSeconds, 6)\"" 2>/dev/null
    fi
}

# Hide cursor
tput civis 2>/dev/null || true
trap "tput cnorm 2>/dev/null; exit" INT TERM EXIT

clear

while true; do
    # Move cursor to top
    tput cup 0 0 2>/dev/null || true

    echo "======================================================================"
    echo "           DanteSync Real-Time Monitoring Dashboard                   "
    echo "======================================================================"
    printf " Time: %-58s\n" "$(date)"
    echo "======================================================================"
    printf " %-15s | %-12s | %-10s | %-15s\n" "Host" "Offset (ms)" "Status" "Last Update"
    echo "----------------------------------------------------------------------"

    # Get reference time
    REF_INFO=${TARGETS[$REF_HOST]}
    REF_USER_HOST=${REF_INFO%%:*}
    REF_OS=${REF_INFO##*:}
    REF_TIME=$(get_time "$REF_USER_HOST" "$REF_OS")

    if [[ -z "$REF_TIME" ]]; then
        printf " %-15s | %12s | %-10s | %-15s\n" "$REF_HOST" "N/A" "OFFLINE" "-"
    fi

    for host in "${!TARGETS[@]}"; do
        info=${TARGETS[$host]}
        user_host=${info%%:*}
        os=${info##*:}

        if [[ "$host" == "$REF_HOST" ]]; then
            if [[ -n "$REF_TIME" ]]; then
                printf " %-15s | %+11.3f | %-10s | %-15s\n" "$host" "0.000" "REFERENCE" "$(date +%H:%M:%S)"
            fi
            continue
        fi

        TIME=$(get_time "$user_host" "$os")

        if [[ -n "$TIME" && -n "$REF_TIME" ]]; then
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
            fi

            printf " %-15s | %+11.3f | %-10s | %-15s\n" "$host" "$DIFF" "$STATUS" "$(date +%H:%M:%S)"
        else
            printf " %-15s | %12s | %-10s | %-15s\n" "$host" "N/A" "OFFLINE" "-"
        fi
    done

    echo "======================================================================"
    echo ""
    echo "Press Ctrl+C to exit"

    sleep "$REFRESH_INTERVAL"
done
