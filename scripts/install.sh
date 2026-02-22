#!/bin/bash
set -euo pipefail

# FlightClock installer for Raspberry Pi Zero 2W + HyperPixel 2.1 Round
# Run as root on the Pi

INSTALL_DIR="/opt/flight-clock"
CONFIG_DIR="/etc/flight-clock"
SERVICE_NAME="flight-clock"
BINARY_NAME="flight-clock"

echo "=== FlightClock Installer ==="
echo ""

# Check if running as root
if [ "$(id -u)" -ne 0 ]; then
    echo "Error: Must run as root (use sudo)"
    exit 1
fi

# --- Step 1: Stop old flight-tracker stack if running ---
echo "[1/7] Stopping old services..."
# Stop Chromium kiosk / Node.js if present
systemctl stop chromium-kiosk 2>/dev/null || true
systemctl disable chromium-kiosk 2>/dev/null || true
systemctl stop flight-tracker 2>/dev/null || true
systemctl disable flight-tracker 2>/dev/null || true
# Kill any lingering chromium/node processes
killall chromium-browser 2>/dev/null || true
killall node 2>/dev/null || true

# --- Step 2: Install system dependencies ---
echo "[2/7] Installing system dependencies..."
apt-get update -qq
apt-get install -y -qq \
    libinput10 \
    libudev1 \
    hostapd \
    dnsmasq \
    wireless-tools

# Disable hostapd and dnsmasq by default (managed by our app)
systemctl disable hostapd 2>/dev/null || true
systemctl stop hostapd 2>/dev/null || true
systemctl disable dnsmasq 2>/dev/null || true
systemctl stop dnsmasq 2>/dev/null || true

# --- Step 3: Create directories ---
echo "[3/7] Creating directories..."
mkdir -p "$INSTALL_DIR"
mkdir -p "$CONFIG_DIR"

# --- Step 4: Install binary ---
echo "[4/7] Installing binary..."
if [ -f "./$BINARY_NAME" ]; then
    cp "./$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
elif [ -f "/tmp/$BINARY_NAME" ]; then
    cp "/tmp/$BINARY_NAME" "$INSTALL_DIR/$BINARY_NAME"
else
    echo "Error: Binary not found."
    echo "  Copy it to the Pi first:"
    echo "    scp target/aarch64-unknown-linux-gnu/release/flight-clock pi@<pi-ip>:/tmp/"
    echo "  Then run this script again."
    exit 1
fi
chmod +x "$INSTALL_DIR/$BINARY_NAME"

# --- Step 5: Create config ---
echo "[5/7] Setting up config..."
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    cat > "$CONFIG_DIR/config.json" << 'EOF'
{
    "latitude": 55.979636,
    "longitude": -3.577456,
    "radius_nm": 6.0,
    "facing_direction": "N"
}
EOF
    chmod 644 "$CONFIG_DIR/config.json"
    echo "  Created default config at $CONFIG_DIR/config.json"
    echo "  >>> Edit this with your coordinates! <<<"
else
    echo "  Config already exists, keeping it."
fi

# --- Step 6: Install and enable systemd service ---
echo "[6/7] Installing systemd service..."
cat > /etc/systemd/system/flight-clock.service << 'SERVICEEOF'
[Unit]
Description=FlightClock Display
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
WorkingDirectory=/opt/flight-clock
ExecStart=/opt/flight-clock/flight-clock
Restart=always
RestartSec=5
WatchdogSec=120
StandardOutput=journal
StandardError=journal
MemoryMax=64M

# Slint LinuxKMS backend with software renderer (no GPU/X11/Wayland needed)
Environment=SLINT_BACKEND=linuxkms-software

# Ensure libinput can find devices for touch input
Environment=XDG_RUNTIME_DIR=/run/user/0

[Install]
WantedBy=multi-user.target
SERVICEEOF

systemctl daemon-reload
systemctl enable "$SERVICE_NAME"

# --- Step 7: Configure system for headless display ---
echo "[7/7] Configuring system..."

# Disable desktop environment if installed (we render directly to framebuffer)
systemctl set-default multi-user.target 2>/dev/null || true

# Configure journald log rotation
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/flight-clock.conf << 'EOF'
[Journal]
SystemMaxUse=50M
SystemMaxFileSize=10M
EOF
systemctl restart systemd-journald

# Ensure the user running the service can access DRM and input devices
# (running as root handles this, but add video/input groups for safety)
usermod -a -G video,input,render root 2>/dev/null || true

echo ""
echo "============================================"
echo "  FlightClock installed successfully!"
echo "============================================"
echo ""
echo "  Binary:   $INSTALL_DIR/$BINARY_NAME"
echo "  Config:   $CONFIG_DIR/config.json"
echo "  Service:  $SERVICE_NAME"
echo ""
echo "  Commands:"
echo "    sudo nano $CONFIG_DIR/config.json   # edit your location"
echo "    sudo systemctl start $SERVICE_NAME  # start the clock"
echo "    sudo systemctl stop $SERVICE_NAME   # stop the clock"
echo "    sudo journalctl -u $SERVICE_NAME -f # view logs"
echo ""

read -p "Start FlightClock now? [Y/n] " -n 1 -r
echo ""
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
    systemctl start "$SERVICE_NAME"
    sleep 2
    systemctl status "$SERVICE_NAME" --no-pager || true
fi
