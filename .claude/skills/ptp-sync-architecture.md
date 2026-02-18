---
name: ptp-sync-architecture
description: Domain knowledge for DanteSync PTP/NTP dual-source clock synchronization architecture
---

# DanteSync PTP Synchronization Architecture

## Core Architecture: Dual-Source Synchronization

DanteSync implements a **hybrid PTP+NTP approach** where each protocol serves a distinct purpose:

| Source    | Protocol  | Purpose                        | System Call                                   | What It Controls    |
| --------- | --------- | ------------------------------ | --------------------------------------------- | ------------------- |
| Dante/PTP | PTPv1 UDP | Frequency sync (syntonization) | `adjtimex` / `SetSystemTimeAdjustmentPrecise` | Clock tick rate     |
| NTP       | NTP UDP   | Phase/time sync                | `settimeofday` / `SetSystemTime`              | Absolute time value |

**Critical Insight:** These operations are INDEPENDENT:

- `step_clock()` sets the time value - does NOT affect frequency
- `adjust_frequency()` sets the tick rate - does NOT affect absolute time

## The Dante Time Domain Problem

### What Dante PTP Actually Sends

Dante devices send **device uptime** as their PTP timestamp, NOT UTC time:

- Grandmaster powered on 3 hours ago → T1 = 10,800 seconds
- This has NO relation to real-world UTC time
- The "1980 epoch" or "IEEE 1588 epoch" is NOT used by Dante

### Implications

1. **PTP offset is meaningless for UTC alignment** - the offset (e.g., +182ms) tells you nothing about real time
2. **Rate of change is what matters** - stable rate = frequencies matched
3. **NTP is required for UTC** - PTP alone cannot provide real-world time

## Servo Algorithm: Rate-Based Control

The controller uses a rate-based servo that tracks the **rate of change** of offset:

```
Drift rate (µs/s) = (offset_t - offset_t-1) / dt
```

| Metric                | Meaning                       | Target    |
| --------------------- | ----------------------------- | --------- |
| Drift rate = 0        | Frequencies perfectly matched | Ideal     |
| Drift rate < 5 µs/s   | LOCK mode achieved            | Good      |
| Drift rate < 0.5 µs/s | NANO mode (ultra-precise)     | Excellent |

### Three-Phase Control

1. **ACQ (Acquisition)**: Fast convergence, aggressive gains
2. **PROD (Production)**: Gentle stability, moderate gains
3. **NANO (Ultra-precise)**: Sub-microsecond stability, minimal corrections

## Accumulated Phase Error Tracking

Between NTP steps, phase error accumulates based on drift rate:

```
accumulated_error += drift_rate_us_s * dt
```

This is reset to 0 after each NTP step. The adaptive NTP interval uses this:

- Error > 50µs → check NTP every 10s
- Error > 20µs → check NTP every 15s
- Otherwise → default 30s interval

## Optimal Deployment: NTP Server Mode

For multi-computer sync, designate ONE DanteSync machine as NTP master:

```
┌─────────────────────────────────────────────────────────────────────────┐
│              "NTP MASTER" Computer                                       │
│  ┌───────────────┐    ┌───────────────┐    ┌───────────────┐            │
│  │   DanteSync   │    │  NTP Server   │    │  System Clock │            │
│  │  (PTP client) │───►│ (builtin)     │◄───│  (disciplined)│            │
│  └───────────────┘    └───────────────┘    └───────────────┘            │
│        ▲                      │                                          │
│        │                      │ Serves PTP-disciplined time              │
└────────┼──────────────────────┼──────────────────────────────────────────┘
         │                      ▼
┌────────┴──────────────────────────────────────────────────────────────────┐
│          Dante PTP Grandmaster (Audinate device)                          │
└───────────────────────────────────────────────────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────────────────┐
│              "CLIENT" Computers                                            │
│  ┌───────────────┐    ┌───────────────┐                                   │
│  │   DanteSync   │───►│ NTP Client    │ ◄── Points to Master              │
│  │  (PTP client) │    │ (dantesync)   │                                   │
│  └───────────────┘    └───────────────┘                                   │
│                                                                            │
│  Result: BOTH frequency AND phase synced to same source!                   │
└───────────────────────────────────────────────────────────────────────────┘
```

**Benefits:**

1. NTP master time = Dante-disciplined time (unified time domain)
2. All clients sync to same source (both freq + phase)
3. No external NTP jitter (LAN-only, sub-ms latency)
4. Eliminates "two masters" problem

## Troubleshooting

### "Drift rate high" or "not locking"

1. Check PTP packets are being received (not blocked by firewall)
2. Verify Dante grandmaster is online and stable
3. Check network jitter - high jitter prevents stable lock

### "NTP offset keeps growing"

1. Normal if not using NTP server mode - Dante frequency may differ from NTP reference
2. Enable NTP server mode on one machine and point others to it
3. The accumulated phase tracking shows this - watch for values > 100µs

### "Sync source changed" messages

1. Normal during Dante grandmaster failover
2. DanteSync does "soft reset" - preserves learned frequency
3. Recovery should be quick (~30s to re-lock)

### PTP offline mode

1. If no PTP packets for 10s, switches to NTP-only mode
2. Orange icon in tray indicates NTP-only
3. Check Dante network connectivity and grandmaster status

## Key Code Locations

| File                   | Component                        | Purpose                             |
| ---------------------- | -------------------------------- | ----------------------------------- |
| `src/controller.rs`    | `PtpController`                  | Main sync logic, servo algorithm    |
| `src/controller.rs`    | `apply_self_tuning_servo()`      | Rate-based frequency control        |
| `src/controller.rs`    | `check_ntp_utc_tracking()`       | Periodic NTP alignment              |
| `src/ntp_server.rs`    | NTP server                       | Built-in NTP server for master mode |
| `src/clock/linux.rs`   | `adjtimex`                       | Linux frequency adjustment          |
| `src/clock/windows.rs` | `SetSystemTimeAdjustmentPrecise` | Windows frequency adjustment        |
