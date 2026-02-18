#!/usr/bin/env python3
"""
sync-snapshot.py - DanteSync Network Time Verification

Queries all DanteSync targets via UDP Time Query API (port 31900) and displays
a comprehensive comparison of clock synchronization across the network.

Usage:
    python3 scripts/sync-snapshot.py              # Full report
    python3 scripts/sync-snapshot.py --brief      # Quick status only
    python3 scripts/sync-snapshot.py --json       # JSON output
    python3 scripts/sync-snapshot.py --hosts X Y  # Query specific hosts
"""

import argparse
import json
import socket
import struct
import sys
import threading
import time
from dataclasses import dataclass, asdict
from datetime import datetime
from typing import Dict, List, Optional

# =============================================================================
# CONFIGURATION - Edit TARGETS to match your network
# =============================================================================

TARGETS = {
    "strih.lan": "10.77.9.202",      # NTP Master
    "develbox": "10.77.9.21",         # Linux
    "stream.lan": "10.77.9.204",
    "mbc.lan": "10.77.9.232",
    "iem": "10.77.9.231",
    "ableton-foh": "10.77.9.230",
    "songs": "10.77.9.212",
    "stagebox1.lan": "10.77.9.237",
    "piano.lan": "10.77.9.236",
}

# Default reference host (NTP master)
DEFAULT_REFERENCE = "strih.lan"

# =============================================================================
# PROTOCOL CONSTANTS
# =============================================================================

PORT = 31900
REQUEST_MAGIC = 0x4453594E  # "DSYN"
RESPONSE_MAGIC = 0x44535952  # "DSYR"
MODES = {0: "INIT", 1: "ACQ", 2: "PROD", 3: "LOCK", 4: "NANO", 5: "NTP-only"}


# =============================================================================
# DATA STRUCTURES
# =============================================================================

@dataclass
class TimeResponse:
    """Complete response from DanteSync UDP Time Query."""
    host: str
    ip: str
    system_time_ns: int
    monotonic_counter: int
    monotonic_freq: int
    ptp_offset_ns: int
    drift_rate_ppm: float
    freq_adj_ppm: float
    mode: str
    is_locked: bool
    gm_uuid: str
    rtt_us: float
    error: Optional[str] = None


# =============================================================================
# QUERY FUNCTIONS
# =============================================================================

def query_target(host: str, ip: str, timeout: float = 0.5) -> TimeResponse:
    """Send UDP time query and parse response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(timeout)

    request_id = int(time.time() * 1000) & 0xFFFFFFFF
    request = struct.pack(">II", REQUEST_MAGIC, request_id)

    try:
        t1 = time.perf_counter_ns()
        sock.sendto(request, (ip, PORT))
        data, _ = sock.recvfrom(64)
        t4 = time.perf_counter_ns()

        if len(data) < 64:
            return TimeResponse(host=host, ip=ip, system_time_ns=0, monotonic_counter=0,
                              monotonic_freq=0, ptp_offset_ns=0, drift_rate_ppm=0,
                              freq_adj_ppm=0, mode="", is_locked=False, gm_uuid="",
                              rtt_us=0, error="Short response")

        magic, resp_id = struct.unpack(">II", data[0:8])
        if magic != RESPONSE_MAGIC or resp_id != request_id:
            return TimeResponse(host=host, ip=ip, system_time_ns=0, monotonic_counter=0,
                              monotonic_freq=0, ptp_offset_ns=0, drift_rate_ppm=0,
                              freq_adj_ppm=0, mode="", is_locked=False, gm_uuid="",
                              rtt_us=0, error="Invalid response")

        system_ns = struct.unpack(">Q", data[8:16])[0]
        mono = struct.unpack(">Q", data[16:24])[0]
        ptp_off = struct.unpack(">q", data[24:32])[0]
        drift_scaled = struct.unpack(">i", data[32:36])[0]
        adj_scaled = struct.unpack(">i", data[36:40])[0]
        mode_byte = data[40]
        locked = data[41]
        gm_uuid = ":".join(f"{b:02X}" for b in data[42:48])
        mono_freq = struct.unpack(">Q", data[48:56])[0]

        return TimeResponse(
            host=host,
            ip=ip,
            system_time_ns=system_ns,
            monotonic_counter=mono,
            monotonic_freq=mono_freq,
            ptp_offset_ns=ptp_off,
            drift_rate_ppm=drift_scaled / 1000.0,
            freq_adj_ppm=adj_scaled / 1000.0,
            mode=MODES.get(mode_byte, f"?{mode_byte}"),
            is_locked=locked == 1,
            gm_uuid=gm_uuid,
            rtt_us=(t4 - t1) / 1000.0,
        )
    except socket.timeout:
        return TimeResponse(host=host, ip=ip, system_time_ns=0, monotonic_counter=0,
                          monotonic_freq=0, ptp_offset_ns=0, drift_rate_ppm=0,
                          freq_adj_ppm=0, mode="", is_locked=False, gm_uuid="",
                          rtt_us=0, error="Timeout")
    except Exception as e:
        return TimeResponse(host=host, ip=ip, system_time_ns=0, monotonic_counter=0,
                          monotonic_freq=0, ptp_offset_ns=0, drift_rate_ppm=0,
                          freq_adj_ppm=0, mode="", is_locked=False, gm_uuid="",
                          rtt_us=0, error=str(e))
    finally:
        sock.close()


def query_all(targets: Dict[str, str], timeout: float = 0.5) -> List[TimeResponse]:
    """Query all targets in parallel."""
    results: Dict[str, TimeResponse] = {}
    threads = []

    def worker(host: str, ip: str):
        results[host] = query_target(host, ip, timeout)

    for host, ip in targets.items():
        t = threading.Thread(target=worker, args=(host, ip))
        threads.append(t)
        t.start()

    for t in threads:
        t.join()

    return [results[h] for h in sorted(results.keys())]


# =============================================================================
# OUTPUT FORMATTERS
# =============================================================================

def print_brief(results: List[TimeResponse], reference: str):
    """Print brief status summary."""
    ref = next((r for r in results if r.host == reference and not r.error), None)
    if not ref:
        ref = next((r for r in results if not r.error), None)
        if ref:
            reference = ref.host

    online = [r for r in results if not r.error]
    offline = [r for r in results if r.error]

    print(f"DanteSync Status: {len(online)}/{len(results)} hosts online")
    print()

    if not ref:
        print("ERROR: No hosts responding!")
        return 1

    max_offset = 0
    all_locked = True
    all_same_gm = True
    ref_gm = ref.gm_uuid

    for r in online:
        if r.host == reference:
            continue
        offset_us = abs(r.system_time_ns - ref.system_time_ns) / 1000
        max_offset = max(max_offset, offset_us)
        if not r.is_locked:
            all_locked = False
        if r.gm_uuid != ref_gm:
            all_same_gm = False

    print(f"{'Host':<14} {'Mode':<6} {'Locked':<8} {'Offset':<12} Status")
    print("-" * 60)

    for r in results:
        if r.error:
            print(f"{r.host:<14} {'--':<6} {'--':<8} {'--':<12} OFFLINE ({r.error})")
            continue

        if r.host == reference:
            print(f"{r.host:<14} {r.mode:<6} {'YES' if r.is_locked else 'no':<8} {'(reference)':<12} REFERENCE")
            continue

        offset_us = (r.system_time_ns - ref.system_time_ns) / 1000
        offset_str = f"{offset_us:+.0f}us"

        if abs(offset_us) < 1000:
            status = "OK"
        elif abs(offset_us) < 5000:
            status = "DRIFT"
        else:
            status = "ERROR"

        print(f"{r.host:<14} {r.mode:<6} {'YES' if r.is_locked else 'no':<8} {offset_str:<12} {status}")

    print()
    if all_locked and all_same_gm and max_offset < 5000:
        print(f"RESULT: ALL SYNCED (max offset: {max_offset:.0f}us)")
        return 0
    else:
        issues = []
        if not all_locked:
            issues.append("not all locked")
        if not all_same_gm:
            issues.append("different grandmasters")
        if max_offset >= 5000:
            issues.append(f"max offset {max_offset/1000:.1f}ms")
        print(f"RESULT: ISSUES DETECTED ({', '.join(issues)})")
        return 1


def print_comprehensive(results: List[TimeResponse], reference: str):
    """Print comprehensive report with all tables."""
    query_time = datetime.now()

    ref = next((r for r in results if r.host == reference and not r.error), None)
    if not ref:
        ref = next((r for r in results if not r.error), None)
        if ref:
            reference = ref.host

    online = [r for r in results if not r.error]

    print("=" * 100)
    print("DANTESYNC NETWORK TIME VERIFICATION")
    print("=" * 100)
    print(f"Query Time: {query_time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"Reference:  {reference}")
    print()

    # Table 1: Sync Status
    print("TABLE 1: SYNC STATUS")
    print("-" * 100)
    print(f"{'Host':<14} {'IP':<15} {'Mode':<6} {'Locked':<8} {'Offset (us)':<14} {'RTT (us)':<10} {'Status':<12}")
    print("-" * 100)

    for r in results:
        if r.error:
            print(f"{r.host:<14} {r.ip:<15} {'--':<6} {'--':<8} {'--':<14} {'--':<10} OFFLINE")
            continue

        offset_us = (r.system_time_ns - ref.system_time_ns) / 1000 if ref else 0
        lock_str = "YES" if r.is_locked else "no"

        if r.host == reference:
            status = "REFERENCE"
        elif abs(offset_us) < 1000:
            status = "SYNCED"
        elif abs(offset_us) < 5000:
            status = "DRIFT"
        else:
            status = "ERROR"

        print(f"{r.host:<14} {r.ip:<15} {r.mode:<6} {lock_str:<8} {offset_us:>+13.1f} {r.rtt_us:>9.0f} {status:<12}")

    print()

    # Table 2: Clock Readings
    print("TABLE 2: CLOCK READINGS")
    print("-" * 100)
    print(f"{'Host':<14} {'System Time (ns)':<24} {'Monotonic Counter':<22} {'Freq':<12} {'Uptime':<10}")
    print("-" * 100)

    for r in results:
        if r.error:
            print(f"{r.host:<14} {'--':<24} {'--':<22} {'--':<12} {'--':<10}")
            continue

        if r.monotonic_freq > 0:
            uptime_hrs = (r.monotonic_counter / r.monotonic_freq) / 3600
            uptime_str = f"{uptime_hrs:.1f}h"
        else:
            uptime_str = "--"

        if r.monotonic_freq >= 1e9:
            freq_str = f"{r.monotonic_freq/1e9:.2f}GHz"
        else:
            freq_str = f"{r.monotonic_freq/1e6:.0f}MHz"

        print(f"{r.host:<14} {r.system_time_ns:<24} {r.monotonic_counter:<22} {freq_str:<12} {uptime_str:<10}")

    print()

    # Table 3: PTP Status
    print("TABLE 3: PTP SERVO STATUS")
    print("-" * 100)
    print(f"{'Host':<14} {'PTP Offset (ns)':<16} {'Drift (us/s)':<14} {'Freq Adj PPM':<14} {'Grandmaster':<20} {'GM Match':<10}")
    print("-" * 100)

    ref_gm = ref.gm_uuid if ref else ""

    for r in results:
        if r.error:
            print(f"{r.host:<14} {'--':<16} {'--':<14} {'--':<14} {'--':<20} {'--':<10}")
            continue

        gm_match = "YES" if r.gm_uuid == ref_gm else "NO"
        print(f"{r.host:<14} {r.ptp_offset_ns:>15} {r.drift_rate_ppm:>+13.2f} {r.freq_adj_ppm:>+13.2f} {r.gm_uuid:<20} {gm_match:<10}")

    print()

    # Summary
    print("SUMMARY")
    print("-" * 100)

    if not ref:
        print("ERROR: No reference host available!")
        return 1

    offsets = [(r.system_time_ns - ref.system_time_ns) / 1000
               for r in online if r.host != reference]

    if offsets:
        max_off = max(offsets, key=abs)
        min_off = min(offsets, key=abs)
        avg_off = sum(offsets) / len(offsets)

        print(f"Hosts Online:     {len(online)}/{len(results)}")
        print(f"Max Offset:       {max_off:+.1f} us ({abs(max_off)/1000:.2f} ms)")
        print(f"Min Offset:       {min_off:+.1f} us")
        print(f"Avg Offset:       {avg_off:+.1f} us")
        print()

        all_locked = all(r.is_locked for r in online)
        all_same_gm = len(set(r.gm_uuid for r in online)) == 1
        all_synced = all(abs(o) < 5000 for o in offsets)

        print(f"All Locked:       {'YES' if all_locked else 'NO'}")
        print(f"Same Grandmaster: {'YES' if all_same_gm else 'NO'}")
        print(f"All Synced (<5ms):{'YES' if all_synced else 'NO'}")
        print("=" * 100)

        return 0 if (all_locked and all_same_gm and all_synced) else 1

    return 1


def print_json(results: List[TimeResponse], reference: str):
    """Print JSON output."""
    ref = next((r for r in results if r.host == reference and not r.error), None)

    output = {
        "query_time": datetime.now().isoformat(),
        "reference": reference,
        "hosts": []
    }

    for r in results:
        host_data = asdict(r)
        if ref and not r.error:
            host_data["offset_us"] = (r.system_time_ns - ref.system_time_ns) / 1000
        output["hosts"].append(host_data)

    online = [r for r in results if not r.error]
    output["summary"] = {
        "online": len(online),
        "total": len(results),
        "all_locked": all(r.is_locked for r in online) if online else False,
        "all_same_gm": len(set(r.gm_uuid for r in online)) == 1 if online else False,
    }

    print(json.dumps(output, indent=2))
    return 0


# =============================================================================
# MAIN
# =============================================================================

def main():
    parser = argparse.ArgumentParser(
        description="DanteSync Network Time Verification",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s                    Full comprehensive report
  %(prog)s --brief            Quick status summary
  %(prog)s --json             JSON output for scripting
  %(prog)s --hosts strih.lan develbox
        """
    )
    parser.add_argument("-r", "--reference", default=DEFAULT_REFERENCE,
                       help=f"Reference host (default: {DEFAULT_REFERENCE})")
    parser.add_argument("-t", "--timeout", type=float, default=0.5,
                       help="Query timeout in seconds (default: 0.5)")
    parser.add_argument("--brief", action="store_true",
                       help="Show brief status only")
    parser.add_argument("--json", action="store_true",
                       help="Output as JSON")
    parser.add_argument("--hosts", nargs="+",
                       help="Query specific hosts only")
    args = parser.parse_args()

    # Select targets
    if args.hosts:
        targets = {}
        for h in args.hosts:
            if h in TARGETS:
                targets[h] = TARGETS[h]
            elif "." in h:  # Looks like IP or hostname
                targets[h] = h
            else:
                print(f"Warning: Unknown host '{h}'", file=sys.stderr)
        if not targets:
            print("Error: No valid hosts specified", file=sys.stderr)
            return 1
    else:
        targets = TARGETS

    # Query all targets
    results = query_all(targets, args.timeout)

    # Output
    if args.json:
        return print_json(results, args.reference)
    elif args.brief:
        return print_brief(results, args.reference)
    else:
        return print_comprehensive(results, args.reference)


if __name__ == "__main__":
    sys.exit(main())
