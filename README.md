# Dante PTP Time Sync (dantetimesync)

A Rust-based tool to synchronize the local system clock with a Dante PTP v1 Grandmaster.
Migrated and improved from an original C++ Windows tool to support both Windows and Linux.

## Features

- **Cross-Platform:** Runs on Windows and Linux.
- **PTP v1 Support:** Listens for Dante PTP v1 multicast packets.
- **Frequency Locking:** Uses frequency adjustment (syntonization) instead of time stepping for smooth audio clock handling.
- **Network Auto-Detection:** Automatically selects the appropriate wired network interface.
- **Robustness:** Handles network jitter and packet loss with filtering.

## Requirements

- **Windows:** Run as **Administrator** (required for `SetSystemTimeAdjustmentPrecise`).
- **Linux:** Run as **Root** (or with `CAP_SYS_TIME` capability) for `adjtimex`.

## Building

```bash
cargo build --release
```

## Usage

```bash
# Run with auto-detected interface
sudo ./target/release/dantetimesync

# Run on specific interface (optional)
# sudo ./target/release/dantetimesync --interface eth0
```

## How it works

1.  **Discovery:** Finds a network interface (preferring wired IPv4).
2.  **Listening:** Joins Multicast Group `224.0.1.129` on UDP ports 319 and 320.
3.  **Syncing:**
    -   Receives `Sync` and `Follow_Up` messages from the Grandmaster.
    -   Calculates the ratio between Master time passage and Slave time passage.
    -   Adjusts the local system clock frequency (slewing) to match the Master.

## License

MIT
