#!/usr/bin/env python3
"""
analyze-sync.py - Analyze DanteSync monitoring data
Usage: python analyze-sync.py sync_monitor_*.csv

Generates reports showing:
- Max/avg/95th percentile spread
- Per-host analysis
- Anomaly detection for large jumps and offline periods
"""

import sys
import csv
from datetime import datetime
from collections import defaultdict
from typing import Optional


def parse_float(value: str) -> Optional[float]:
    """Parse a float value, returning None for N/A or invalid."""
    try:
        if value.strip().upper() == "N/A":
            return None
        return float(value)
    except (ValueError, AttributeError):
        return None


def percentile(data: list, p: float) -> float:
    """Calculate percentile of a list."""
    if not data:
        return 0.0
    sorted_data = sorted(data)
    k = (len(sorted_data) - 1) * p / 100
    f = int(k)
    c = f + 1 if f + 1 < len(sorted_data) else f
    return sorted_data[f] + (k - f) * (sorted_data[c] - sorted_data[f])


def analyze_sync_data(csv_file: str) -> None:
    """Analyze a sync monitoring CSV file and print report."""
    print(f"\n{'='*60}")
    print(f"DanteSync Analysis Report: {csv_file}")
    print(f"{'='*60}\n")

    # Read CSV data
    rows = []
    hosts = []
    with open(csv_file, "r") as f:
        reader = csv.DictReader(f)
        hosts = [
            col
            for col in reader.fieldnames
            if col
            not in [
                "timestamp",
                "reference_host",
                "max_diff_ms",
                "min_diff_ms",
                "spread_ms",
            ]
        ]
        for row in reader:
            rows.append(row)

    if not rows:
        print("No data found in CSV file.")
        return

    # Extract timestamps and calculate duration
    timestamps = [float(row["timestamp"]) for row in rows]
    duration_hours = (max(timestamps) - min(timestamps)) / 3600

    print(f"Duration: {duration_hours:.2f} hours")
    print(f"Samples: {len(rows)}")
    if len(timestamps) > 1:
        intervals = [timestamps[i + 1] - timestamps[i] for i in range(len(timestamps) - 1)]
        avg_interval = sum(intervals) / len(intervals)
        print(f"Interval: ~{avg_interval:.1f}s\n")

    # Extract spread values
    spreads = [
        parse_float(row.get("spread_ms", "0")) for row in rows if row.get("spread_ms")
    ]
    spreads = [s for s in spreads if s is not None]

    if spreads:
        print("=== SYNC QUALITY METRICS ===\n")
        print(f"Max time spread:     {max(spreads):.3f} ms")
        print(f"Avg time spread:     {sum(spreads)/len(spreads):.3f} ms")
        print(f"95th percentile:     {percentile(spreads, 95):.3f} ms")
        print(f"99th percentile:     {percentile(spreads, 99):.3f} ms")

    # Per-host analysis
    print("\n=== PER-HOST ANALYSIS ===\n")
    host_data = defaultdict(list)

    for row in rows:
        for host in hosts:
            value = parse_float(row.get(host))
            if value is not None:
                host_data[host].append(value)

    for host in hosts:
        data = host_data[host]
        if data:
            mean = sum(data) / len(data)
            variance = sum((x - mean) ** 2 for x in data) / len(data)
            std = variance**0.5
            max_abs = max(abs(x) for x in data)
            outliers = sum(1 for x in data if abs(x) > 10)
            print(
                f"{host:15} | Mean: {mean:+7.3f}ms | Std: {std:6.3f}ms | "
                f"Max: {max_abs:6.3f}ms | Outliers: {outliers}"
            )
        else:
            print(f"{host:15} | No valid data")

    # Detect anomalies
    print("\n=== ANOMALIES DETECTED ===\n")

    # Large jumps (potential GM failover or reboot)
    if len(spreads) > 1:
        large_jumps = []
        for i in range(1, len(spreads)):
            jump = abs(spreads[i] - spreads[i - 1])
            if jump > 5:  # >5ms jump
                ts = datetime.fromtimestamp(timestamps[i])
                large_jumps.append((ts, jump))

        if large_jumps:
            print(f"Large time jumps (>5ms): {len(large_jumps)}")
            for ts, jump in large_jumps[:5]:
                print(f"  - {ts}: spread jumped by {jump:.3f}ms")
        else:
            print("No large time jumps detected")

    # Offline periods (N/A values)
    offline_detected = False
    for host in hosts:
        offline_count = sum(1 for row in rows if row.get(host, "").strip().upper() == "N/A")
        if offline_count > 0:
            offline_detected = True
            print(f"{host} was offline for {offline_count} samples")

    if not offline_detected:
        print("No offline periods detected")

    # Summary
    print("\n=== SUMMARY ===\n")
    if spreads:
        avg_spread = sum(spreads) / len(spreads)
        if avg_spread < 1:
            print(f"EXCELLENT: Average spread {avg_spread:.3f}ms (sub-millisecond)")
        elif avg_spread < 5:
            print(f"GOOD: Average spread {avg_spread:.3f}ms")
        elif avg_spread < 10:
            print(f"ACCEPTABLE: Average spread {avg_spread:.3f}ms")
        else:
            print(f"POOR: Average spread {avg_spread:.3f}ms - investigation needed")


def main():
    if len(sys.argv) < 2:
        print("Usage: python analyze-sync.py <csv_file>")
        print("\nAnalyzes DanteSync monitoring CSV data and generates a report.")
        sys.exit(1)

    analyze_sync_data(sys.argv[1])


if __name__ == "__main__":
    main()
