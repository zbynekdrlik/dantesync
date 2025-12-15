# Dante PTP Time Sync

A high-precision PTP (Precision Time Protocol) synchronization tool optimized for Dante Audio networks, written in Rust.

## Features
- **PTPv1 Support:** Syncs with Dante Grandmasters (PTPv1/UDP 319/320).
- **Hybrid Mode:** Uses NTP for initial coarse alignment, then PTP for microsecond-precision frequency adjustment.
- **Cross-Platform:** Runs on Linux and Windows.
- **Hardware Control:** Adjusts system clock frequency directly (via `adjtimex` on Linux, `SetSystemTimeAdjustmentPrecise` on Windows).
- **Resilient:** Filters "lucky packets" to minimize network jitter effects.

## Installation

### Linux
Run the following command to install the latest version as a system service:
```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/setup.sh | sudo bash
```

### Windows
1.  **Prerequisite:** Install [Npcap](https://npcap.com/#download) (Select "Install Npcap in WinPcap API-compatible Mode").
2.  Open PowerShell as **Administrator**.
3.  Run the following command to install the service and tray app:
```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/install.ps1 | iex
```

## Usage (Manual)
```bash
dantetimesync [OPTIONS]
```
- `--interface <NAME>`: Bind to specific interface (e.g., `eth0`).
- `--ntp-server <IP>`: NTP server for initial sync (default: `10.77.8.2`).
- `--skip-ntp`: Skip NTP sync.
- `--service`: (Windows Only) Run as a Windows Service.

## Build from Source
```bash
cargo build --release
```
**Windows Build Requirements:**
- Rust Toolchain (`x86_64-pc-windows-msvc`)
- WinPcap Developer's Pack (extracted and `LIB` env var set to `WpdPack/Lib/x64`).

## License
MIT