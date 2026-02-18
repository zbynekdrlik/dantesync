#!/usr/bin/env python3
"""
sync-snapshot.py - DanteSync Audio-Grade Sync Verification

Queries all DanteSync targets via UDP Time Query API (port 31900) and displays
audio-meaningful synchronization metrics. Reports drift rates in samples/sec
at the configured sample rate, with verdicts calibrated to Dante audio requirements.

The KEY metric is drift_rate (smoothed_rate_ppm) — the rate of clock error in us/s.
A drift rate < 0.5 us/s (NANO mode) means sub-sample precision at 96kHz.
Wall clock offsets measured via UDP have ~1ms uncertainty and CANNOT verify sub-sample sync.

Usage:
    python3 scripts/sync-snapshot.py                    # Full report
    python3 scripts/sync-snapshot.py --brief            # Quick status only
    python3 scripts/sync-snapshot.py --json             # JSON output
    python3 scripts/sync-snapshot.py --sample-rate 48000
    python3 scripts/sync-snapshot.py --hosts X Y        # Query specific hosts
"""

import argparse
import json
import socket
import struct
import sys
import threading
import time
from dataclasses import dataclass, asdict, field
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
# AUDIO SYNC THRESHOLDS (from controller.rs constants)
# =============================================================================

DRIFT_NANO_US = 0.5     # NANO mode entry: < 0.5 us/s
DRIFT_LOCK_US = 5.0     # LOCK threshold: < 5 us/s
DRIFT_PROD_US = 20.0    # PROD/ACQ boundary: < 20 us/s


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
    # NTP fields (protocol v2 — bytes [56-63])
    ntp_offset_us: int = 0
    accumulated_phase_us: float = 0.0
    ntp_failed: bool = False
    settled: bool = False
    has_ntp_fields: bool = False  # True if remote sent non-zero NTP data
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

    error_resp = lambda err: TimeResponse(
        host=host, ip=ip, system_time_ns=0, monotonic_counter=0,
        monotonic_freq=0, ptp_offset_ns=0, drift_rate_ppm=0,
        freq_adj_ppm=0, mode="", is_locked=False, gm_uuid="",
        rtt_us=0, error=err)

    try:
        t1 = time.perf_counter_ns()
        sock.sendto(request, (ip, PORT))
        data, _ = sock.recvfrom(64)
        t4 = time.perf_counter_ns()

        if len(data) < 64:
            return error_resp("Short response")

        magic, resp_id = struct.unpack(">II", data[0:8])
        if magic != RESPONSE_MAGIC or resp_id != request_id:
            return error_resp("Invalid response")

        system_ns = struct.unpack(">Q", data[8:16])[0]
        mono = struct.unpack(">Q", data[16:24])[0]
        ptp_off = struct.unpack(">q", data[24:32])[0]
        drift_scaled = struct.unpack(">i", data[32:36])[0]
        adj_scaled = struct.unpack(">i", data[36:40])[0]
        mode_byte = data[40]
        locked = data[41]
        gm_uuid = ":".join(f"{b:02X}" for b in data[42:48])
        mono_freq = struct.unpack(">Q", data[48:56])[0]

        # Parse NTP fields from bytes [56-63]
        ntp_off_us = struct.unpack(">i", data[56:60])[0]
        accum_phase = struct.unpack(">h", data[60:62])[0]
        flags = data[62]
        ntp_failed = bool(flags & 0x01)
        settled = bool(flags & 0x02)

        # Detect whether remote has NTP fields (all-zero = old version)
        has_ntp = (ntp_off_us != 0 or accum_phase != 0 or flags != 0)

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
            ntp_offset_us=ntp_off_us,
            accumulated_phase_us=float(accum_phase),
            ntp_failed=ntp_failed,
            settled=settled,
            has_ntp_fields=has_ntp,
        )
    except socket.timeout:
        return error_resp("Timeout")
    except Exception as e:
        return error_resp(str(e))
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
# AUDIO QUALITY HELPERS
# =============================================================================

def sample_period_us(sample_rate: int) -> float:
    """Duration of one audio sample in microseconds."""
    return 1_000_000.0 / sample_rate


def drift_to_samples_per_sec(drift_us_per_sec: float, sample_rate: int) -> float:
    """Convert drift rate (us/s) to samples of error per second."""
    return abs(drift_us_per_sec) / sample_period_us(sample_rate)


def audio_quality_rating(r: TimeResponse, sample_rate: int) -> str:
    """Rate sync quality in audio-meaningful terms."""
    if r.error:
        return "OFFLINE"
    drift = abs(r.drift_rate_ppm)
    if not r.is_locked:
        return "DRIFTING"
    if drift < DRIFT_NANO_US:
        return "SAMPLE-LOCKED"
    if drift < DRIFT_LOCK_US:
        return "GOOD"
    if drift < DRIFT_PROD_US:
        return "MARGINAL"
    return "DRIFTING"


def time_to_one_sample_error(drift_us_per_sec: float, sample_rate: int) -> Optional[float]:
    """Seconds until 1 full sample of accumulated error. None if drift ~0."""
    if abs(drift_us_per_sec) < 0.001:
        return None  # effectively zero drift
    return sample_period_us(sample_rate) / abs(drift_us_per_sec)


# =============================================================================
# OUTPUT FORMATTERS - HELPERS
# =============================================================================

def format_human_time(ns: int) -> str:
    """Convert nanoseconds-since-epoch to human-readable HH:MM:SS.mmm"""
    secs = ns / 1e9
    dt = datetime.fromtimestamp(secs)
    frac_ms = (ns % 1_000_000_000) // 1_000_000
    return f"{dt.strftime('%H:%M:%S')}.{frac_ms:03d}"


def format_uptime(counter: int, freq: int) -> str:
    """Convert monotonic counter to human-readable uptime."""
    if freq <= 0:
        return "--"
    total_secs = counter / freq
    days = int(total_secs // 86400)
    hours = int((total_secs % 86400) // 3600)
    mins = int((total_secs % 3600) // 60)
    if days > 0:
        return f"{days}d {hours}h {mins}m"
    elif hours > 0:
        return f"{hours}h {mins}m"
    else:
        return f"{mins}m"


def format_freq(freq: int) -> str:
    """Format monotonic frequency as human-readable string."""
    if freq >= 1e9:
        return f"{freq/1e9:.2f} GHz"
    elif freq >= 1e6:
        return f"{freq/1e6:.0f} MHz"
    elif freq >= 1e3:
        return f"{freq/1e3:.0f} kHz"
    return str(freq)


def format_ns_offset(ns: int) -> str:
    """Format a nanosecond offset as the most readable unit."""
    abs_ns = abs(ns)
    sign = "+" if ns >= 0 else "-"
    if abs_ns >= 1_000_000_000:
        return f"{sign}{abs_ns / 1e9:.3f} s"
    elif abs_ns >= 1_000_000:
        return f"{sign}{abs_ns / 1e6:.3f} ms"
    elif abs_ns >= 1_000:
        return f"{sign}{abs_ns / 1e3:.1f} us"
    return f"{sign}{abs_ns} ns"


W = 120  # output width


# =============================================================================
# BRIEF OUTPUT
# =============================================================================

def print_brief(results: List[TimeResponse], reference: str, sample_rate: int):
    """Print brief status summary with audio-grade metrics."""
    ref = next((r for r in results if r.host == reference and not r.error), None)
    if not ref:
        ref = next((r for r in results if not r.error), None)
        if ref:
            reference = ref.host

    online = [r for r in results if not r.error]
    offline = [r for r in results if r.error]
    sp_us = sample_period_us(sample_rate)

    print(f"DanteSync Status: {len(online)}/{len(results)} hosts online  "
          f"[{sample_rate/1000:.0f}kHz, 1 sample = {sp_us:.1f}us]")
    print()

    if not ref:
        print("ERROR: No hosts responding!")
        return 1

    ref_gm = ref.gm_uuid

    print(f"{'Host':<14} {'Mode':<6} {'Lock':<6} {'Drift us/s':<12} {'Samp/sec':<10} {'Quality':<14} Status")
    print("-" * 80)

    for r in results:
        if r.error:
            print(f"{r.host:<14} {'--':<6} {'--':<6} {'--':<12} {'--':<10} {'OFFLINE':<14} {r.error}")
            continue

        lock_str = "YES" if r.is_locked else "no"
        drift = abs(r.drift_rate_ppm)
        samp_sec = drift_to_samples_per_sec(r.drift_rate_ppm, sample_rate)
        quality = audio_quality_rating(r, sample_rate)
        gm_note = "" if r.gm_uuid == ref_gm else " GM-DIFF!"

        if r.host == reference:
            status = "REFERENCE"
        elif quality == "SAMPLE-LOCKED":
            status = "OK"
        elif quality == "GOOD":
            status = "OK"
        elif quality == "MARGINAL":
            status = "WARN"
        else:
            status = "PROBLEM"

        print(f"{r.host:<14} {r.mode:<6} {lock_str:<6} {r.drift_rate_ppm:>+10.2f}  "
              f"{samp_sec:>8.3f}  {quality:<14} {status}{gm_note}")

    print()

    # Verdict
    max_drift = max((abs(r.drift_rate_ppm) for r in online), default=0)
    all_locked = all(r.is_locked for r in online)
    all_same_gm = len(set(r.gm_uuid for r in online)) == 1
    max_samp_sec = drift_to_samples_per_sec(max_drift, sample_rate)

    if all_locked and all_same_gm and max_drift < DRIFT_NANO_US:
        print(f"VERDICT: SAMPLE-LOCKED (max drift {max_drift:.2f} us/s = {max_samp_sec:.3f} samples/sec)")
        return 0
    elif all_locked and all_same_gm and max_drift < DRIFT_LOCK_US:
        print(f"VERDICT: FREQUENCY-LOCKED (max drift {max_drift:.2f} us/s = {max_samp_sec:.3f} samples/sec)")
        return 0
    else:
        issues = []
        if not all_locked:
            issues.append("not all locked")
        if not all_same_gm:
            issues.append("different grandmasters")
        if max_drift >= DRIFT_LOCK_US:
            issues.append(f"drift {max_drift:.1f} us/s = {max_samp_sec:.1f} samp/sec")
        print(f"VERDICT: {'DEGRADED' if all_locked else 'NOT SYNCED'} ({', '.join(issues)})")
        return 1


# =============================================================================
# COMPREHENSIVE OUTPUT
# =============================================================================

def print_comprehensive(results: List[TimeResponse], reference: str, sample_rate: int):
    """Print comprehensive report with audio-grade metrics."""
    query_time = datetime.now()
    sp_us = sample_period_us(sample_rate)

    ref = next((r for r in results if r.host == reference and not r.error), None)
    if not ref:
        ref = next((r for r in results if not r.error), None)
        if ref:
            reference = ref.host

    online = [r for r in results if not r.error]
    offline = [r for r in results if r.error]
    ref_gm = ref.gm_uuid if ref else ""

    # ── Header ──────────────────────────────────────────────────────────────
    print("=" * W)
    print("DANTESYNC AUDIO-GRADE SYNC VERIFICATION REPORT")
    print("=" * W)
    print(f"  Query Time:    {query_time.strftime('%Y-%m-%d %H:%M:%S')}")
    print(f"  Sample Rate:   {sample_rate:,} Hz (1 sample = {sp_us:.1f} us)")
    print(f"  Reference:     {reference} ({TARGETS.get(reference, '?')})")
    print(f"  Hosts:         {len(online)} online, {len(offline)} offline, {len(results)} total")
    print()
    print("  KEY INSIGHT: The proof of audio sync is in the servo internals (drift_rate, mode),")
    print("  NOT in wall clock offsets measured via UDP (which have ~1ms uncertainty).")
    print("  A drift rate < 0.5 us/s (NANO mode) = sub-sample precision at 96kHz.")
    print("=" * W)
    print()

    # ── Table 1: Audio Sync Quality (THE headline table) ───────────────────
    print("TABLE 1: AUDIO SYNC QUALITY")
    print(f"  How well each host maintains sample-accurate sync at {sample_rate/1000:.0f}kHz.")
    print(f"  Drift Rate = rate of clock error (us/s) — the KEY metric for audio sync.")
    print(f"  Samples/sec = drift expressed as audio samples of error per second.")
    print(f"  Thresholds: SAMPLE-LOCKED < 0.5 us/s | GOOD < 5 us/s | MARGINAL < 20 us/s | DRIFTING")
    print("-" * W)
    hdr = (f"{'Host':<15}{'Mode':<7}{'Lock':<6}{'Drift (us/s)':<14}"
           f"{'Samp/sec':<11}{'Time to 1 samp':<16}{'Quality':<15}{'Settled':<8}")
    print(hdr)
    print("-" * W)

    for r in results:
        if r.error:
            print(f"{r.host:<15}{'--':<7}{'--':<6}{'--':<14}{'--':<11}{'--':<16}{'OFFLINE':<15}{'--':<8}")
            continue

        lock_str = "YES" if r.is_locked else "no"
        drift = abs(r.drift_rate_ppm)
        samp_sec = drift_to_samples_per_sec(r.drift_rate_ppm, sample_rate)
        quality = audio_quality_rating(r, sample_rate)
        t_one = time_to_one_sample_error(r.drift_rate_ppm, sample_rate)

        if t_one is None:
            time_str = "inf"
        elif t_one >= 3600:
            time_str = f"{t_one/3600:.1f}h"
        elif t_one >= 60:
            time_str = f"{t_one/60:.1f}m"
        else:
            time_str = f"{t_one:.1f}s"

        settled_str = "YES" if r.settled else ("--" if not r.has_ntp_fields else "no")

        print(f"{r.host:<15}{r.mode:<7}{lock_str:<6}{r.drift_rate_ppm:>+12.2f}  "
              f"{samp_sec:>9.3f}  {time_str:>14}  {quality:<15}{settled_str:<8}")

    print()

    # ── Table 2: Frequency Discipline (servo internals) ────────────────────
    print("TABLE 2: FREQUENCY DISCIPLINE")
    print("  Internal servo state — proves frequency synchronization independently of wall clock.")
    print("  Drift Rate:  smoothed rate of clock error (us/s). Near 0 = clocks tick at same rate.")
    print("  Freq Adj:    correction applied to system clock (PPM). +ve = clock was slow.")
    print("  PTP Offset:  phase vs Dante grandmaster (device-uptime, NOT UTC — large values normal).")
    print("-" * W)
    hdr = (f"{'Host':<15}{'Drift (us/s)':<14}{'Freq Adj PPM':<14}"
           f"{'PTP Offset':<18}{'Raw (ns)':<16}{'Mode':<7}{'GM UUID':<22}{'GM':<5}")
    print(hdr)
    print("-" * W)

    for r in results:
        if r.error:
            print(f"{r.host:<15}{'--':<14}{'--':<14}{'--':<18}{'--':<16}{'--':<7}{'--':<22}--")
            continue

        offset_human = format_ns_offset(r.ptp_offset_ns)
        gm_match = "OK" if r.gm_uuid == ref_gm else "DIFF"

        print(f"{r.host:<15}{r.drift_rate_ppm:>+12.2f}  {r.freq_adj_ppm:>+12.2f}  "
              f"{offset_human:<18}{r.ptp_offset_ns:<16}{r.mode:<7}{r.gm_uuid:<22}{gm_match:<5}")

    print()

    # ── Table 3: UTC Time Alignment (NTP + wall clock) ─────────────────────
    print("TABLE 3: UTC TIME ALIGNMENT")
    print("  Wall Offset:     host system_time minus reference, measured via UDP. Includes ~1ms of")
    print("                   network round-trip noise — CANNOT verify sub-sample precision.")
    print("  NTP Offset:      measured locally on each host (accurate). Requires DanteSync with")
    print("                   expanded UDP protocol; shows '--' for older versions.")
    print("  Accum Phase:     estimated UTC drift since last NTP step (resets to 0 after NTP step).")
    print("-" * W)
    hdr = (f"{'Host':<15}{'Wall Offset':<16}{'+-Uncertainty':<14}"
           f"{'NTP Offset':<13}{'Accum Phase':<13}{'NTP OK':<8}{'RTT (us)':<10}")
    print(hdr)
    print("-" * W)

    for r in results:
        if r.error:
            print(f"{r.host:<15}{'--':<16}{'--':<14}{'--':<13}{'--':<13}{'--':<8}{'--':<10}")
            continue

        if r.host == reference:
            wall_str = "(reference)"
            unc_str = ""
        elif ref:
            offset_us = (r.system_time_ns - ref.system_time_ns) / 1000
            wall_str = f"{offset_us:+.1f} us"
            half_rtt = r.rtt_us / 2
            unc_str = f"+/-{half_rtt:.0f} us"
        else:
            wall_str = "?"
            unc_str = ""

        if r.has_ntp_fields:
            ntp_str = f"{r.ntp_offset_us:+d} us"
            phase_str = f"{r.accumulated_phase_us:+.0f} us"
            ntp_ok = "FAIL" if r.ntp_failed else "OK"
        else:
            ntp_str = "--"
            phase_str = "--"
            ntp_ok = "--"

        print(f"{r.host:<15}{wall_str:<16}{unc_str:<14}"
              f"{ntp_str:<13}{phase_str:<13}{ntp_ok:<8}{r.rtt_us:>8.0f}")

    print()

    # ── Table 4: Hardware & Network ────────────────────────────────────────
    print("TABLE 4: HARDWARE & NETWORK")
    print("  Monotonic counters (never adjusted), platform detection, and network RTT.")
    print("-" * W)
    hdr = (f"{'Host':<15}{'Platform':<10}{'Counter':<24}"
           f"{'Frequency':<14}{'Uptime':<14}{'RTT (us)':<10}{'GM UUID':<22}")
    print(hdr)
    print("-" * W)

    for r in results:
        if r.error:
            print(f"{r.host:<15}{'--':<10}{'--':<24}{'--':<14}{'--':<14}{'--':<10}--")
            continue

        platform = "Linux" if r.monotonic_freq == 1_000_000_000 else "Windows"
        freq_str = format_freq(r.monotonic_freq)
        uptime = format_uptime(r.monotonic_counter, r.monotonic_freq)

        print(f"{r.host:<15}{platform:<10}{r.monotonic_counter:<24}"
              f"{freq_str:<14}{uptime:<14}{r.rtt_us:>8.0f}  {r.gm_uuid}")

    print()

    # ── Summary ─────────────────────────────────────────────────────────────
    print("=" * W)
    print("SUMMARY")
    print("-" * W)

    if not ref:
        print("  ERROR: No reference host available!")
        return 1

    if len(online) < 2:
        print("  Only 1 host online — nothing to compare.")
        return 1

    # Compute audio-relevant stats
    all_locked = all(r.is_locked for r in online)
    all_same_gm = len(set(r.gm_uuid for r in online)) == 1
    max_drift = max((abs(r.drift_rate_ppm) for r in online), default=0)
    max_drift_host = max(online, key=lambda r: abs(r.drift_rate_ppm))
    max_samp_sec = drift_to_samples_per_sec(max_drift, sample_rate)
    t_one = time_to_one_sample_error(max_drift, sample_rate)

    print(f"  Hosts responding:    {len(online)}/{len(results)}")
    if offline:
        print(f"  Offline hosts:       {', '.join(r.host for r in offline)}")
    print()
    print(f"  Audio Sync @ {sample_rate/1000:.0f}kHz (1 sample = {sp_us:.1f} us):")
    print(f"    Max drift rate:    {max_drift:.2f} us/s  ({max_drift_host.host})")
    print(f"    As samples/sec:    {max_samp_sec:.3f} samples/sec of error")
    if t_one is not None:
        if t_one >= 3600:
            print(f"    Time to 1 sample:  {t_one/3600:.1f} hours until 1 sample of accumulated error")
        elif t_one >= 60:
            print(f"    Time to 1 sample:  {t_one/60:.1f} minutes until 1 sample of accumulated error")
        else:
            print(f"    Time to 1 sample:  {t_one:.1f} seconds until 1 sample of accumulated error")
    else:
        print(f"    Time to 1 sample:  infinite (drift effectively zero)")
    print()
    print(f"  Servo Health:")
    print(f"    All locked:        {'YES' if all_locked else 'NO  <-- some hosts not locked!'}")
    print(f"    Same grandmaster:  {'YES' if all_same_gm else 'NO  <-- network segmentation!'}")

    # Per-host quality breakdown
    qualities = {}
    for r in online:
        q = audio_quality_rating(r, sample_rate)
        qualities.setdefault(q, []).append(r.host)
    print(f"    Quality breakdown: ", end="")
    parts = []
    for q in ["SAMPLE-LOCKED", "GOOD", "MARGINAL", "DRIFTING"]:
        if q in qualities:
            parts.append(f"{len(qualities[q])} {q}")
    print(", ".join(parts))
    print()

    # Verdict
    if all_locked and all_same_gm and max_drift < DRIFT_NANO_US:
        print(f"  VERDICT: SAMPLE-LOCKED")
        print(f"  All {len(online)} hosts in NANO/LOCK with drift < {DRIFT_NANO_US} us/s.")
        print(f"  Sub-sample precision at {sample_rate/1000:.0f}kHz — suitable for Dante audio.")
        result = 0
    elif all_locked and all_same_gm and max_drift < DRIFT_LOCK_US:
        print(f"  VERDICT: FREQUENCY-LOCKED")
        print(f"  All hosts locked, max drift {max_drift:.2f} us/s = {max_samp_sec:.3f} samples/sec.")
        print(f"  Operationally fine — less than half a sample per second of drift.")
        result = 0
    elif all_locked and all_same_gm:
        print(f"  VERDICT: DEGRADED")
        print(f"  All hosts locked but max drift {max_drift:.1f} us/s = {max_samp_sec:.1f} samples/sec.")
        marginal = qualities.get("MARGINAL", []) + qualities.get("DRIFTING", [])
        if marginal:
            print(f"  Problem hosts: {', '.join(marginal)}")
        result = 1
    else:
        print(f"  VERDICT: NOT SYNCED")
        issues = []
        if not all_locked:
            unlocked = [r.host for r in online if not r.is_locked]
            issues.append(f"Unlocked: {', '.join(unlocked)}")
        if not all_same_gm:
            gms = set(r.gm_uuid for r in online)
            issues.append(f"Multiple grandmasters: {len(gms)}")
        for issue in issues:
            print(f"    - {issue}")
        result = 1

    print("=" * W)
    return result


# =============================================================================
# JSON OUTPUT
# =============================================================================

def print_json(results: List[TimeResponse], reference: str, sample_rate: int):
    """Print JSON output with audio-grade metrics."""
    sp_us = sample_period_us(sample_rate)
    ref = next((r for r in results if r.host == reference and not r.error), None)

    output = {
        "query_time": datetime.now().isoformat(),
        "reference": reference,
        "sample_rate": sample_rate,
        "sample_period_us": sp_us,
        "hosts": []
    }

    for r in results:
        host_data = asdict(r)
        if not r.error:
            drift = abs(r.drift_rate_ppm)
            host_data["drift_samples_per_sec"] = drift_to_samples_per_sec(r.drift_rate_ppm, sample_rate)
            host_data["audio_quality"] = audio_quality_rating(r, sample_rate)
            t_one = time_to_one_sample_error(r.drift_rate_ppm, sample_rate)
            host_data["time_to_one_sample_sec"] = t_one
            if ref:
                host_data["wall_offset_us"] = (r.system_time_ns - ref.system_time_ns) / 1000
        output["hosts"].append(host_data)

    online = [r for r in results if not r.error]
    max_drift = max((abs(r.drift_rate_ppm) for r in online), default=0)
    all_locked = all(r.is_locked for r in online) if online else False
    all_same_gm = (len(set(r.gm_uuid for r in online)) == 1) if online else False

    if all_locked and all_same_gm and max_drift < DRIFT_NANO_US:
        verdict = "SAMPLE-LOCKED"
    elif all_locked and all_same_gm and max_drift < DRIFT_LOCK_US:
        verdict = "FREQUENCY-LOCKED"
    elif all_locked and all_same_gm:
        verdict = "DEGRADED"
    else:
        verdict = "NOT SYNCED"

    output["summary"] = {
        "online": len(online),
        "total": len(results),
        "all_locked": all_locked,
        "all_same_gm": all_same_gm,
        "max_drift_us_per_sec": max_drift,
        "max_drift_samples_per_sec": drift_to_samples_per_sec(max_drift, sample_rate),
        "verdict": verdict,
    }

    print(json.dumps(output, indent=2))
    return 0


# =============================================================================
# MAIN
# =============================================================================

def main():
    parser = argparse.ArgumentParser(
        description="DanteSync Audio-Grade Sync Verification",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s                         Full comprehensive report
  %(prog)s --brief                 Quick status summary
  %(prog)s --json                  JSON output for scripting
  %(prog)s --sample-rate 48000     Show thresholds for 48kHz
  %(prog)s --hosts strih.lan develbox
        """
    )
    parser.add_argument("-r", "--reference", default=DEFAULT_REFERENCE,
                       help=f"Reference host (default: {DEFAULT_REFERENCE})")
    parser.add_argument("-t", "--timeout", type=float, default=0.5,
                       help="Query timeout in seconds (default: 0.5)")
    parser.add_argument("-s", "--sample-rate", type=int, default=96000,
                       choices=[44100, 48000, 96000, 192000],
                       help="Audio sample rate in Hz (default: 96000)")
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
        return print_json(results, args.reference, args.sample_rate)
    elif args.brief:
        return print_brief(results, args.reference, args.sample_rate)
    else:
        return print_comprehensive(results, args.reference, args.sample_rate)


if __name__ == "__main__":
    sys.exit(main())
