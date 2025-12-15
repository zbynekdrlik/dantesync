# Dante PTP Time Sync (dantetimesync)

A state-of-the-art Rust service to synchronize the local system clock with a Dante PTP v1 Grandmaster, ensuring both absolute time accuracy via NTP and microsecond-level frequency locking via PTP.

## Features

- **Hybrid Sync:** Steps clock on startup via NTP (default: `10.77.8.2`), then frequency locks to Dante PTP (`224.0.1.129`).
- **SOTA Precision:** Uses **Kernel Timestamping (SO_TIMESTAMPNS)** and a tuned **PI Servo** to achieve **< 50µs phase offset**.
- **Fast Lock:** Intelligent stepping logic aligns phase instantly.
- **Hardware Integration:** Updates RTC via `ioctl`. Runs with **Realtime Priority (SCHED_FIFO)**.
- **Robustness:** Singleton locking and 100% Rust unit test coverage.

## Quick Install (One-Line)

Run this command on your Ubuntu/Debian machine to install and start the service automatically:

```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/setup.sh | sudo bash
```

### Custom NTP Server

To specify a custom NTP server during installation:

```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/setup.sh | sudo NTP_SERVER="pool.ntp.org" bash
```

### Changing NTP Server After Installation

Edit the systemd service file:

1.  Open the service file:
    ```bash
    sudo nano /etc/systemd/system/dantetimesync.service
    ```
2.  Find the `ExecStart` line and change the `--ntp-server` argument:
    ```ini
    ExecStart=/usr/local/bin/dantetimesync --ntp-server 192.168.1.1
    ```
3.  Reload and restart:
    ```bash
    sudo systemctl daemon-reload
    sudo systemctl restart dantetimesync
    ```

## Usage

Check status:
```bash
sudo systemctl status dantetimesync
```

View logs:
```bash
sudo journalctl -u dantetimesync -f
```

## Manual Build (Dev)

```bash
git clone https://github.com/zbynekdrlik/dantetimesync.git
cd dantetimesync
sudo ./install.sh
```

## Architecture

- `src/main.rs`: Entry point, Singleton Lock, Service Wiring.
- `src/controller.rs`: Core control loop logic (`PtpController`).
- `src/servo.rs`: PI Servo implementation.
- `src/rtc.rs`: Direct RTC hardware access via `ioctl`.
- `src/ptp.rs`: PTP v1 packet parsing.
- `src/net.rs`: Network interface and socket management (Kernel Timestamps).

## License

MIT
