#!/bin/bash
set -euo pipefail

# FlightClock installer for Raspberry Pi Zero 2W
# Run as root on the Pi

INSTALL_DIR="/opt/flight-clock"
CONFIG_DIR="/etc/flight-clock"
SERVICE_NAME="flight-clock"
BINARY_NAME="flight-clock"

echo "=== FlightClock Installer ==="

# Check if running as root
if [ "$(id -u)" -ne 0 ]; then
    echo "Error: Must run as root"
    exit 1
fi

# Create directories
echo "Creating directories..."
mkdir -p "$INSTALL_DIR"
mkdir -p "$CONFIG_DIR"

# Copy binary
if [ -f "./$BINARY_NAME" ]; then
    echo "Installing binary..."
    cp "./$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"
elif [ -f "./target/aarch64-unknown-linux-gnu/release/$BINARY_NAME" ]; then
    echo "Installing cross-compiled binary..."
    cp "./target/aarch64-unknown-linux-gnu/release/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"
else
    echo "Error: Binary not found. Build first with: cargo build --release"
    exit 1
fi

# Create default config if not exists
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    echo "Creating default config..."
    cat > "$CONFIG_DIR/config.json" << 'EOF'
{
    "latitude": 55.979636,
    "longitude": -3.577456,
    "radius_nm": 6.0,
    "facing_direction": "N"
}
EOF
    chmod 644 "$CONFIG_DIR/config.json"
fi

# Install systemd service
echo "Installing systemd service..."
cp ./systemd/flight-clock.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

# Install required packages for AP mode
echo "Installing dependencies..."
apt-get update -qq
apt-get install -y -qq hostapd dnsmasq wireless-tools

# Disable hostapd and dnsmasq by default (managed by our app)
systemctl disable hostapd 2>/dev/null || true
systemctl stop hostapd 2>/dev/null || true
systemctl disable dnsmasq 2>/dev/null || true
systemctl stop dnsmasq 2>/dev/null || true

# Configure journald log rotation
echo "Configuring log rotation..."
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/flight-clock.conf << 'EOF'
[Journal]
SystemMaxUse=50M
SystemMaxFileSize=10M
EOF
systemctl restart systemd-journald

echo ""
echo "=== Installation complete ==="
echo ""
echo "Edit config:  nano $CONFIG_DIR/config.json"
echo "Start:        systemctl start $SERVICE_NAME"
echo "Status:       systemctl status $SERVICE_NAME"
echo "Logs:         journalctl -u $SERVICE_NAME -f"
echo ""
echo "Starting FlightClock..."
systemctl start "$SERVICE_NAME"
systemctl status "$SERVICE_NAME" --no-pager
