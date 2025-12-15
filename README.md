# Dante PTP Time Sync (dantetimesync)

A state-of-the-art Rust service to synchronize the local system clock with a Dante PTP v1 Grandmaster, ensuring both absolute time accuracy via NTP and microsecond-level frequency locking via PTP.

## Features

- **Hybrid Sync:** Steps clock on startup via NTP (`10.77.8.2`), then frequency locks to Dante PTP (`224.0.1.129`).
- **SOTA Precision:** Uses **Kernel Timestamping (SO_TIMESTAMPNS)** and a tuned **PI Servo** to achieve **< 50µs phase offset**.
- **Fast Lock:** Intelligent stepping logic aligns phase instantly.
- **Hardware Integration:** Updates RTC via `ioctl`. Runs with **Realtime Priority (SCHED_FIFO)**.
- **Robustness:** 100% Rust unit test coverage.

## Quick Install (One-Line)

Run this command on your Ubuntu/Debian machine to install and start the service automatically:

```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/setup.sh | sudo bash
```

This will:
1.  Download the latest binary release.
2.  Install dependencies (util-linux).
3.  Set up and start the systemd service.

## Check Status

```bash
sudo systemctl status dantetimesync
sudo journalctl -u dantetimesync -f
```

## Manual Build (Dev)

```bash
git clone https://github.com/zbynekdrlik/dantetimesync.git
cd dantetimesync
sudo ./install.sh
```

## License

MIT