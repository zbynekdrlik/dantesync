# Dante PTP Time Sync (dantetimesync)

A state-of-the-art Rust service to synchronize the local system clock with a Dante PTP v1 Grandmaster, ensuring both absolute time accuracy via NTP and microsecond-level frequency locking via PTP.

## Features

- **Hybrid Sync:**
    - **Phase 1 (NTP):** Steps the clock on startup using a specified NTP server (default: `10.77.8.2`) to ensure correct wall-clock time.
    - **Phase 2 (PTP):** Locks to Dante PTP v1 multicast (224.0.1.129) for precision frequency drift correction.
- **Cross-Platform:** Runs on Linux and Windows.
- **Robustness:** 100% Test Coverage of core logic using Mocking and TDD.
- **Architecture:** Senior-level dependency injection pattern (`PtpController` with swappable Network/Clock/NTP backends).

## Requirements

- **Windows:** Run as **Administrator** (required for `SetSystemTimeAdjustmentPrecise` and `SetSystemTime`).
- **Linux:** Run as **Root** (or with `CAP_SYS_TIME` capability) for `adjtimex` and `settimeofday`.

## Building

```bash
cargo build --release
```

## Testing

The project uses `mockall` for comprehensive unit testing.

```bash
cargo test
```

## Usage

```bash
# Run with default settings (NTP: 10.77.8.2, Auto Interface)
sudo ./target/release/dantetimesync

# Specify Interface
sudo ./target/release/dantetimesync --interface eth0

# Specify NTP Server
sudo ./target/release/dantetimesync --ntp-server pool.ntp.org

# Skip NTP (PTP Only)
sudo ./target/release/dantetimesync --skip-ntp
```

## Architecture

- `src/main.rs`: Entry point and wiring of concrete implementations (`RealPtpNetwork`, `RealNtpSource`).
- `src/controller.rs`: Core control loop logic (`PtpController`), fully unit-tested with mocks.
- `src/clock/`: Platform-specific system clock control (`adjtimex` for Linux, `SetSystemTimeAdjustmentPrecise` for Windows).
- `src/ptp.rs`: PTP v1 packet parsing.
- `src/ntp.rs`: NTP client wrapper using `rsntp`.
- `src/net.rs`: Network interface selection and multicast socket creation.

## License

MIT