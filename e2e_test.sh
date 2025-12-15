#!/bin/bash
set -e

# Build
echo "Building release binary..."
cargo build --release

BIN="./target/release/dantetimesync"
LOG_FILE="e2e_test.log"

echo "Starting dantetimesync (requires sudo)..."
# Run for 30 seconds
# We use 'timeout' to kill it automatically
sudo timeout --preserve-status --signal=SIGINT 30s $BIN --interface enp1s0 > $LOG_FILE 2>&1 || true

echo "Analyzing logs..."

# Check for NTP
if grep -q "NTP Offset:" $LOG_FILE; then
    echo "[PASS] NTP Sync attempted."
else
    echo "[FAIL] NTP Sync did not run."
fi

if grep -q "Clock stepped successfully" $LOG_FILE || grep -q "Clock offset small" $LOG_FILE; then
    echo "[PASS] NTP Sync completed."
else
    echo "[FAIL] NTP Sync completion not found (maybe failed?)."
fi

# Check for PTP
if grep -q "Received Sync Seq" $LOG_FILE; then
    echo "[PASS] PTP Packets received."
else
    echo "[FAIL] No PTP Packets received."
fi

if grep -q "Adjustment active" $LOG_FILE; then
    echo "[PASS] Clock sync locked (Adjustment active)."
else
    echo "[FAIL] Clock sync did not lock within 30s."
    echo "Last 10 lines of log:"
    tail -n 10 $LOG_FILE
    exit 1
fi

echo "E2E Test Passed!"
exit 0
