# Dante PTP Time Sync (dantetimesync)

A state-of-the-art Rust service to synchronize the local system clock with a Dante PTP v1 Grandmaster, ensuring both absolute time accuracy via NTP and microsecond-level frequency locking via PTP.

## Features

- **Hybrid Sync:**
    - **Phase 1 (NTP):** Steps the clock on startup using a specified NTP server (default: `10.77.8.2`) to ensure correct wall-clock time.
    - **Phase 2 (PTP):** Locks to Dante PTP v1 multicast (224.0.1.129) for precision frequency drift correction.
- **SOTA Precision:**
    - **Kernel Timestamping (SO_TIMESTAMPNS):** Eliminates userspace scheduling jitter.
    - **PI Servo:** Tuned PI controller (`Kp=0.0005`, `Ki=0.00005`) eliminates steady-state error.
    - **Lucky Packet Filter:** Statistically rejects network queuing delay.
- **Hardware Integration:**
    - **RTC Update:** Writes to `/dev/rtc0` via `ioctl` to persist time across reboots.
    - **Realtime Priority:** Sets `SCHED_FIFO` priority 50 for low latency.
- **Cross-Platform:** Runs on Linux (optimized) and Windows.

## Installation (Ubuntu/Debian)

Run the following command to install dependencies, build the service, and start it automatically:

```bash
git clone https://github.com/zbynekdrlik/dantetimesync.git
cd dantetimesync
sudo ./install.sh
```

This script will:
1.  Install Rust and build tools.
2.  Compile the release binary.
3.  Install it to `/usr/local/bin/dantetimesync`.
4.  Disable conflicting services (`systemd-timesyncd`, `chrony`).
5.  Install and start the `dantetimesync` systemd service.

## Usage

Check status:
```bash
sudo systemctl status dantetimesync
```

View logs:
```bash
sudo journalctl -u dantetimesync -f
```

## Manual Build

```bash
cargo build --release
```

## Architecture

- `src/main.rs`: Entry point and wiring.
- `src/controller.rs`: Core control loop logic (`PtpController`).
- `src/servo.rs`: PI Servo implementation.
- `src/rtc.rs`: Direct RTC hardware access.
- `src/ptp.rs`: PTP v1 packet parsing.
- `src/net.rs`: Network interface and socket management.

## License

MIT
