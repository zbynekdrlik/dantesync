#!/bin/bash
set -e

echo ">>> Dante Time Sync Installer <<<"

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo ./install.sh)"
  exit 1
fi

# 1. Install System Dependencies
echo ">>> Installing system dependencies..."
apt-get update
# util-linux provides hwclock (if available on platform)
apt-get install -y build-essential curl util-linux

# 2. Install Rust (if missing)
if ! command -v cargo &> /dev/null; then
    echo ">>> Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env" || source "/root/.cargo/env"
else
    echo ">>> Rust is already installed."
fi

# 3. Build Release Binary
echo ">>> Building dantetimesync..."
# Ensure cargo is in path if we just installed it or are in sudo
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
cargo build --release

# 4. Install Binary
echo ">>> Installing binary to /usr/local/bin/..."
cp target/release/dantetimesync /usr/local/bin/
chmod +x /usr/local/bin/dantetimesync

# 5. Disable Conflicting Services
echo ">>> Disabling conflicting time services..."
# We disable these to prevent them from fighting for the clock
systemctl stop systemd-timesyncd 2>/dev/null || true
systemctl disable systemd-timesyncd 2>/dev/null || true
systemctl stop chrony 2>/dev/null || true
systemctl disable chrony 2>/dev/null || true
systemctl stop ntp 2>/dev/null || true
systemctl disable ntp 2>/dev/null || true

# 6. Create Systemd Service
echo ">>> Creating systemd service..."
cat <<EOF > /etc/systemd/system/dantetimesync.service
[Unit]
Description=Dante PTP Time Sync Service
After=network-online.target
Wants=network-online.target

[Service]
# Run as root for port 319, adjtimex, and RTC ioctl access
User=root
Group=root
ExecStart=/usr/local/bin/dantetimesync
Restart=always
RestartSec=5
# High priority for timestamping accuracy (Redundant with internal code but good practice)
CPUSchedulingPolicy=fifo
CPUSchedulingPriority=50

[Install]
WantedBy=multi-user.target
EOF

# 7. Enable and Start Service
echo ">>> Starting service..."
systemctl daemon-reload
systemctl enable dantetimesync
systemctl restart dantetimesync

echo ">>> Installation Complete!"
echo ">>> Check status with: systemctl status dantetimesync"
echo ">>> View logs with: journalctl -u dantetimesync -f"
