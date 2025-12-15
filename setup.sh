#!/bin/bash
set -e

REPO="zbynekdrlik/dantetimesync"
BIN_NAME="dantetimesync"
INSTALL_DIR="/usr/local/bin"
SERVICE_FILE="/etc/systemd/system/dantetimesync.service"

echo ">>> Dante Time Sync Web Installer <<<"

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo bash ...)"
  exit 1
fi

# 1. Determine Download URL (Latest Release)
echo ">>> Fetching latest release info..."
LATEST_URL=$(curl -s "https://api.github.com/repos/$REPO/releases/latest" | grep "browser_download_url" | grep "$BIN_NAME\"" | cut -d '"' -f 4)

if [ -z "$LATEST_URL" ]; then
    echo "Error: Could not find release asset '$BIN_NAME' in $REPO."
    echo "Using fallback to master branch raw binary (if available) or aborting."
    exit 1
fi

# 2. Install System Dependencies (Runtime only)
echo ">>> Installing runtime dependencies..."
apt-get update -qq
# util-linux for hwclock (optional but good)
apt-get install -y -qq util-linux curl

# 3. Download Binary
echo ">>> Downloading $BIN_NAME from $LATEST_URL..."
curl -L -o "$INSTALL_DIR/$BIN_NAME" "$LATEST_URL"
chmod +x "$INSTALL_DIR/$BIN_NAME"

# 4. Disable Conflicting Services
echo ">>> Disabling conflicting time services..."
systemctl stop systemd-timesyncd 2>/dev/null || true
systemctl disable systemd-timesyncd 2>/dev/null || true
systemctl stop chrony 2>/dev/null || true
systemctl disable chrony 2>/dev/null || true
systemctl stop ntp 2>/dev/null || true
systemctl disable ntp 2>/dev/null || true

# 5. Create Systemd Service
echo ">>> Creating systemd service..."
cat <<EOF > "$SERVICE_FILE"
[Unit]
Description=Dante PTP Time Sync Service
After=network-online.target
Wants=network-online.target

[Service]
User=root
Group=root
ExecStart=$INSTALL_DIR/$BIN_NAME
Restart=always
RestartSec=5
# Realtime Priority
CPUSchedulingPolicy=fifo
CPUSchedulingPriority=50

[Install]
WantedBy=multi-user.target
EOF

# 6. Enable and Start
echo ">>> Starting service..."
systemctl daemon-reload
systemctl enable dantetimesync
systemctl restart dantetimesync

echo ">>> Installation Complete!"
echo "Status: systemctl status dantetimesync"
echo "Logs:   journalctl -u dantetimesync -f"
