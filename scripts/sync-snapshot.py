#!/usr/bin/env python3
"""
sync-snapshot.py - Parallel UDP time query to all DanteSync targets

Returns snapshot-in-time comparison of all computers' clock state by querying
the UDP Time Query API (port 31900) on each target simultaneously.

This provides independent verification that all computers have the same tick
counter value - not just the same rate, but the same absolute time.

Protocol:
- Request: 8 bytes (magic "DSYN" + request_id)
- Response: 64 bytes (see time_server.rs for format)
"""

import argparse
import socket
import struct
import sys
import threading
import time
from dataclasses import dataclass
from typing import Dict, Optional

# Target inventory from TARGETS.md
TARGETS = {
    "develbox": "10.77.9.21",
    "iem": "10.77.9.231",
    "ableton-foh": "10.77.9.230",
    "mbc.lan": "10.77.9.232",
    "strih.lan": "10.77.9.202",
    "stream.lan": "10.77.9.204",
    "songs": "10.77.9.212",
    "stagebox1.lan": "10.77.9.237",
    "piano.lan": "10.77.9.236",
}

# Protocol constants
PORT = 31900
REQUEST_MAGIC = 0x4453594E  # "DSYN"
RESPONSE_MAGIC = 0x44535952  # "DSYR"
REQUEST_SIZE = 8
RESPONSE_SIZE = 64

# Mode names
MODES = {0: "INIT", 1: "ACQ", 2: "PROD", 3: "LOCK", 4: "NANO", 5: "NTP-only"}


@dataclass
class TimeResponse:
    """Response from a DanteSync time query."""
    system_time_ns: int      # UTC nanoseconds since Unix epoch
    monotonic_counter: int   # Raw counter value (QPC or CLOCK_MONOTONIC_RAW)
    ptp_offset_ns: int       # PTP offset from grandmaster (nanoseconds)
    drift_rate_ppm: float    # Smoothed drift rate (PPM)
    freq_adj_ppm: float      # Current frequency adjustment (PPM)
    mode: str                # Sync mode (ACQ, PROD, LOCK, NANO, NTP-only)
    is_locked: bool          # True if frequency is locked
    gm_uuid: bytes           # Grandmaster UUID (6 bytes)
    mono_freq: int           # Monotonic counter frequency (Hz)
    rtt_us: float            # Round-trip time for this query (microseconds)


def query_target(host: str, ip: str, timeout: float = 0.5) -> Optional[TimeResponse]:
    """Send UDP time query and parse response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)

    # Generate request ID from current time
    request_id = int(time.time() * 1000) & 0xFFFFFFFF
    request = struct.pack(">II", REQUEST_MAGIC, request_id)

    try:
        t1 = time.perf_counter_ns()
        sock.sendto(request, (ip, PORT))
        data, _ = sock.recvfrom(RESPONSE_SIZE)
        t4 = time.perf_counter_ns()

        if len(data) < RESPONSE_SIZE:
            return None

        # Parse response header
        magic, resp_id = struct.unpack(">II", data[0:8])
        if magic != RESPONSE_MAGIC or resp_id != request_id:
            return None

        # Parse response body
        system_ns = struct.unpack(">Q", data[8:16])[0]
        mono = struct.unpack(">Q", data[16:24])[0]
        ptp_off = struct.unpack(">q", data[24:32])[0]  # signed
        drift_scaled = struct.unpack(">i", data[32:36])[0]  # signed
        adj_scaled = struct.unpack(">i", data[36:40])[0]  # signed
        mode_byte = data[40]
        locked = data[41]
        gm_uuid = data[42:48]
        mono_freq = struct.unpack(">Q", data[48:56])[0]

        return TimeResponse(
            system_time_ns=system_ns,
            monotonic_counter=mono,
            ptp_offset_ns=ptp_off,
            drift_rate_ppm=drift_scaled / 1000.0,
            freq_adj_ppm=adj_scaled / 1000.0,
            mode=MODES.get(mode_byte, f"?{mode_byte}"),
            is_locked=locked == 1,
            gm_uuid=gm_uuid,
            mono_freq=mono_freq,
            rtt_us=(t4 - t1) / 1000.0,
        )
    except socket.timeout:
        return None
    except Exception as e:
        print(f"Error querying {host}: {e}", file=sys.stderr)
        return None
    finally:
        sock.close()


def parallel_query(targets: Dict[str, str], timeout: float = 0.5) -> Dict[str, Optional[TimeResponse]]:
    """Query all targets in parallel."""
    results: Dict[str, Optional[TimeResponse]] = {}
    threads = []

    def worker(host: str, ip: str):
        results[host] = query_target(host, ip, timeout)

    for host, ip in targets.items():
        t = threading.Thread(target=worker, args=(host, ip))
        threads.append(t)
        t.start()

    for t in threads:
        t.join()

    return results


def format_uuid(uuid_bytes: bytes) -> str:
    """Format 6-byte UUID as MAC-style string."""
    return ":".join(f"{b:02X}" for b in uuid_bytes)


def analyze_results(results: Dict[str, Optional[TimeResponse]], reference_host: str = "strih.lan"):
    """Analyze and display results."""
    print("=" * 90)
    print("DanteSync Network Time Snapshot")
    print("=" * 90)
    print()

    # Raw data table
    print(f"{'Host':<15} {'System Time (s)':<20} {'Mode':<8} {'Locked':<8} {'RTT':<10}")
    print("-" * 90)

    valid_results = {h: r for h, r in results.items() if r is not None}
    offline_hosts = [h for h, r in results.items() if r is None]

    for host in sorted(results.keys()):
        r = results[host]
        if r is None:
            print(f"{host:<15} {'OFFLINE':<20}")
            continue

        # Convert nanoseconds to seconds with fractional part
        sys_sec = r.system_time_ns // 1_000_000_000
        sys_frac = r.system_time_ns % 1_000_000_000
        sys_time_str = f"{sys_sec}.{sys_frac:09d}"

        locked_str = "YES" if r.is_locked else "no"
        print(f"{host:<15} {sys_time_str:<20} {r.mode:<8} {locked_str:<8} {r.rtt_us:>6.0f} us")

    print()

    if not valid_results:
        print("No responses received! Check that:")
        print("  1. DanteSync is running on target machines")
        print("  2. UDP port 31900 is not blocked by firewall")
        print("  3. Network connectivity to targets")
        return 1

    # Find reference host
    if reference_host not in valid_results:
        # Fall back to first available host
        reference_host = sorted(valid_results.keys())[0]
        print(f"Note: Using {reference_host} as reference (strih.lan not available)")
        print()

    ref = valid_results[reference_host]

    # Offset analysis
    print("=" * 90)
    print(f"OFFSET ANALYSIS (relative to {reference_host})")
    print("=" * 90)
    print()
    print(f"{'Host':<15} {'System dt':<14} {'Mono dt (norm)':<16} {'GM UUID':<20} {'Status'}")
    print("-" * 90)

    max_offset_us = 0
    all_synced = True

    for host in sorted(valid_results.keys()):
        r = valid_results[host]

        if host == reference_host:
            gm_str = format_uuid(r.gm_uuid) if any(r.gm_uuid) else "(none)"
            print(f"{host:<15} {'(reference)':<14} {'(reference)':<16} {gm_str:<20} REFERENCE")
            continue

        # System time difference (should be ~0 if NTP working)
        sys_diff_us = (r.system_time_ns - ref.system_time_ns) / 1000.0

        # Monotonic difference - normalize to nanoseconds using frequency
        # Each host may have different counter frequency
        if ref.mono_freq > 0 and r.mono_freq > 0:
            ref_mono_ns = (ref.monotonic_counter * 1_000_000_000) // ref.mono_freq
            r_mono_ns = (r.monotonic_counter * 1_000_000_000) // r.mono_freq
            mono_diff_us = (r_mono_ns - ref_mono_ns) / 1000.0
        else:
            mono_diff_us = 0.0

        # Determine status based on system time offset
        abs_offset = abs(sys_diff_us)
        max_offset_us = max(max_offset_us, abs_offset)

        if abs_offset < 1000:  # <1ms
            status = "SYNCED"
            status_icon = "+"
        elif abs_offset < 5000:  # <5ms
            status = "DRIFT"
            status_icon = "~"
            all_synced = False
        else:
            status = "ERROR"
            status_icon = "!"
            all_synced = False

        gm_str = format_uuid(r.gm_uuid) if any(r.gm_uuid) else "(none)"
        print(f"{host:<15} {sys_diff_us:+12.1f}us {mono_diff_us:+14.1f}us {gm_str:<20} {status_icon} {status}")

    print()
    print("-" * 90)

    # Summary
    online_count = len(valid_results)
    total_count = len(results)
    print(f"Summary: {online_count}/{total_count} hosts responding")
    print(f"Max offset: {max_offset_us:.1f} us")

    if offline_hosts:
        print(f"Offline: {', '.join(sorted(offline_hosts))}")

    if all_synced:
        print("Status: ALL HOSTS SYNCED (within 1ms)")
        return 0
    else:
        print("Status: SYNC ISSUES DETECTED")
        return 1


def main():
    parser = argparse.ArgumentParser(
        description="Query DanteSync targets for network time verification"
    )
    parser.add_argument(
        "-r", "--reference",
        default="strih.lan",
        help="Reference host for offset calculation (default: strih.lan)"
    )
    parser.add_argument(
        "-t", "--timeout",
        type=float,
        default=0.5,
        help="UDP query timeout in seconds (default: 0.5)"
    )
    parser.add_argument(
        "-v", "--verbose",
        action="store_true",
        help="Show additional debug information"
    )
    parser.add_argument(
        "--hosts",
        nargs="+",
        help="Query specific hosts only (default: all from TARGETS)"
    )
    args = parser.parse_args()

    # Select targets
    if args.hosts:
        targets = {h: TARGETS.get(h, h) for h in args.hosts}
        # If host looks like an IP, use it directly
        for h in args.hosts:
            if h not in TARGETS and "." in h:
                targets[h] = h
    else:
        targets = TARGETS

    if args.verbose:
        print(f"Querying {len(targets)} targets with {args.timeout}s timeout...")
        print()

    results = parallel_query(targets, args.timeout)
    return analyze_results(results, args.reference)


if __name__ == "__main__":
    sys.exit(main())
