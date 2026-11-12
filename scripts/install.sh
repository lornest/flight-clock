#!/bin/bash
set -euo pipefail

# FlightClock installer for Raspberry Pi Zero 2W + HyperPixel 2.1 Round.
# Run as root on the Pi, from an unpacked release (or with a dev binary).
#
#   sudo ./install.sh            # from an unpacked release folder
#
# Installs into a versioned layout so the updater can switch and roll back:
#   /opt/flight-clock/releases/<version>/   binary + scripts
#   /opt/flight-clock/current -> releases/<version>
#   /opt/flight-clock/flight-clock -> current/flight-clock   (systemd runs this)
#
# Re-running is safe. Set FLIGHT_CLOCK_NONINTERACTIVE=1 to skip prompts
# (used by get.sh).

INSTALL_DIR="/opt/flight-clock"
CONFIG_DIR="/etc/flight-clock"
STATE_DIR="/var/lib/flight-clock"
SERVICE_NAME="flight-clock"
# The settings page / WiFi portal run as this unprivileged user
WEB_USER="flight-clock-web"
NONINTERACTIVE="${FLIGHT_CLOCK_NONINTERACTIVE:-0}"

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== FlightClock Installer ==="
echo ""

if [ "$(id -u)" -ne 0 ]; then
    echo "Error: Must run as root (use sudo)"
    exit 1
fi

# Find the binary: unpacked release, or a dev build copied next to this script / to /tmp
if [ -f "$SRC_DIR/flight-clock" ]; then
    BINARY="$SRC_DIR/flight-clock"
elif [ -f "/tmp/flight-clock" ]; then
    BINARY="/tmp/flight-clock"
else
    echo "Error: flight-clock binary not found next to install.sh or in /tmp."
    echo "  Use a release: curl -fsSL https://raw.githubusercontent.com/lornest/flight-clock/main/scripts/get.sh | sudo bash"
    echo "  Or for a dev build: scripts/cross-build.sh, then copy target/pi/flight-clock to /tmp on the Pi."
    exit 1
fi
chmod +x "$BINARY"
if [ -f "$SRC_DIR/VERSION" ]; then
    VERSION="$(tr -d '[:space:]' < "$SRC_DIR/VERSION")"
else
    VERSION="dev"
fi
echo "  Installing version: $VERSION"

# --- Step 1: Stop the old flight-tracker stack if running ---
echo "[1/8] Stopping old services..."
systemctl stop chromium-kiosk 2>/dev/null || true
systemctl disable chromium-kiosk 2>/dev/null || true
systemctl stop flight-tracker 2>/dev/null || true
systemctl disable flight-tracker 2>/dev/null || true
killall chromium-browser 2>/dev/null || true
killall node 2>/dev/null || true

# --- Step 2: System dependencies ---
echo "[2/8] Installing system dependencies..."
apt-get update -qq
# Runtime libraries the binary links against (Slint's linuxkms backend)
apt-get install -y -qq libinput10 libudev1 libxkbcommon0 libgbm1 libfontconfig1

# WiFi setup hotspot. The app uses NetworkManager when it's running (Pi OS
# Bookworm and later), else wpa_supplicant + hostapd + dnsmasq.
if systemctl is-active --quiet NetworkManager; then
    echo "  NetworkManager detected"
    # NM's "shared" mode runs its own dnsmasq for the hotspot's DHCP/DNS
    apt-get install -y -qq dnsmasq-base
else
    echo "  NetworkManager not running; using wpa_supplicant/hostapd"
    apt-get install -y -qq hostapd dnsmasq wireless-tools
fi

# The app starts hostapd/dnsmasq itself when needed. A system-wide dnsmasq
# (e.g. left over from the old flight-tracker hotspot script) would hold
# ports 53/67 and break the setup hotspot.
systemctl disable --now hostapd 2>/dev/null || true
systemctl disable --now dnsmasq 2>/dev/null || true

# --- Step 3: Directories ---
echo "[3/8] Creating directories..."
mkdir -p "$INSTALL_DIR/releases" "$CONFIG_DIR" "$STATE_DIR"
# Readable by the web server's user (it shows the update status); no secrets here
chmod 755 "$STATE_DIR"
if ! id -u "$WEB_USER" >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin "$WEB_USER"
fi
# Clocks installed before the web server had its own user: keep phones paired
# by copying their sessions to where it now keeps them
if [ -f "$CONFIG_DIR/web-sessions.json" ] && [ ! -f /var/lib/flight-clock-web/web-sessions.json ]; then
    install -d -m700 -o "$WEB_USER" -g "$WEB_USER" /var/lib/flight-clock-web
    install -m600 -o "$WEB_USER" -g "$WEB_USER" "$CONFIG_DIR/web-sessions.json" /var/lib/flight-clock-web/web-sessions.json
fi

# --- Step 4: Install this release ---
echo "[4/8] Installing release $VERSION..."
RELEASE_DIR="$INSTALL_DIR/releases/$VERSION"
if [ "$SRC_DIR" != "$RELEASE_DIR" ]; then
    rm -rf "$RELEASE_DIR.staging"
    mkdir -p "$RELEASE_DIR.staging"
    install -m755 "$BINARY" "$RELEASE_DIR.staging/flight-clock"
    for item in VERSION install.sh setup-hyperpixel2r.sh systemd migrations keys LICENSE README.md CHANGELOG.md; do
        if [ -e "$SRC_DIR/$item" ]; then
            cp -a "$SRC_DIR/$item" "$RELEASE_DIR.staging/"
        fi
    done
    # Dev install from the repo's scripts/ folder: unit files live one level up
    if [ ! -d "$RELEASE_DIR.staging/systemd" ] && [ -d "$SRC_DIR/../systemd" ]; then
        cp -a "$SRC_DIR/../systemd" "$RELEASE_DIR.staging/"
    fi
    rm -rf "$RELEASE_DIR"
    mv "$RELEASE_DIR.staging" "$RELEASE_DIR"
fi

# Keep whatever was current as "previous" (for rollback)
if [ -L "$INSTALL_DIR/current" ]; then
    OLD_CURRENT="$(readlink "$INSTALL_DIR/current")"
    if [ "$OLD_CURRENT" != "releases/$VERSION" ]; then
        ln -sfn "$OLD_CURRENT" "$INSTALL_DIR/previous"
    fi
fi
ln -sfn "releases/$VERSION" "$INSTALL_DIR/current"
# Older installs had a plain binary here: replace it with the symlink
if [ -e "$INSTALL_DIR/flight-clock" ] && [ ! -L "$INSTALL_DIR/flight-clock" ]; then
    rm -f "$INSTALL_DIR/flight-clock"
fi
ln -sfn "current/flight-clock" "$INSTALL_DIR/flight-clock"

# A fresh install already includes everything this release's migrations do
if [ -d "$RELEASE_DIR/migrations" ]; then
    for m in "$RELEASE_DIR"/migrations/*.sh; do
        [ -e "$m" ] || continue
        name="$(basename "$m")"
        grep -qxF "$name" "$STATE_DIR/migrations-applied" 2>/dev/null || echo "$name" >> "$STATE_DIR/migrations-applied"
    done
fi

# --- Step 5: Config ---
echo "[5/8] Setting up config..."
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    # No location: the clock shows no flights until one is set on the settings page
    cat > "$CONFIG_DIR/config.json" << 'EOF'
{
    "radius_nm": 6.0,
    "facing_direction": "N"
}
EOF
    chmod 644 "$CONFIG_DIR/config.json"
    echo "  Created default config at $CONFIG_DIR/config.json"
    echo "  Set your location on the settings page (see below)."
else
    echo "  Config already exists, keeping it."
fi

# --- Step 6: systemd units ---
echo "[6/8] Installing systemd units..."
for unit in flight-clock.service flight-clock-web.service flight-clock-update@.service flight-clock-update.timer; do
    if [ -f "$RELEASE_DIR/systemd/$unit" ]; then
        install -m644 "$RELEASE_DIR/systemd/$unit" "/etc/systemd/system/$unit"
    else
        echo "  Warning: $unit not found in $RELEASE_DIR/systemd"
    fi
done
systemctl daemon-reload
systemctl enable "$SERVICE_NAME" >/dev/null 2>&1
# Started and stopped together with the clock (see the unit file)
systemctl enable flight-clock-web.service >/dev/null 2>&1
systemctl enable --now flight-clock-update.timer >/dev/null 2>&1 || true

# --- Step 7: System settings ---
echo "[7/8] Configuring system..."
# No desktop: the clock draws straight to the display
systemctl set-default multi-user.target >/dev/null 2>&1 || true

# Keep logs small
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/flight-clock.conf << 'EOF'
[Journal]
SystemMaxUse=50M
SystemMaxFileSize=10M
EOF
systemctl restart systemd-journald

usermod -a -G video,input,render root 2>/dev/null || true

# --- Step 8: Tidy old releases (keep current, previous and dev) ---
echo "[8/8] Tidying old releases..."
KEEP_PREV="$(readlink "$INSTALL_DIR/previous" 2>/dev/null || true)"
for dir in "$INSTALL_DIR"/releases/*; do
    [ -d "$dir" ] || continue
    rel="releases/$(basename "$dir")"
    if [ "$rel" != "releases/$VERSION" ] && [ "$rel" != "$KEEP_PREV" ] && [ "$rel" != "releases/dev" ]; then
        rm -rf "$dir"
    fi
done

HOST="$(hostname 2>/dev/null || echo flight-clock)"
echo ""
echo "============================================"
echo "  FlightClock $VERSION installed"
echo "============================================"
echo ""
echo "  Settings:  http://$HOST.local  (pair your phone with the code on the clock)"
echo "  Updates:   daily, automatic (change on the settings page)"
echo "  Logs:      sudo journalctl -u $SERVICE_NAME -u flight-clock-web -f"
echo ""
echo "  Display: for a HyperPixel 2.1 Round, also run (once):"
echo "    sudo $RELEASE_DIR/setup-hyperpixel2r.sh && sudo reboot"
echo ""
echo "  WiFi: if the Pi isn't on a network at boot, the clock opens a"
echo "  'FlightClock-Setup' hotspot and shows setup steps on screen."
echo ""

if [ "$NONINTERACTIVE" = "1" ]; then
    systemctl restart "$SERVICE_NAME"
else
    read -p "Start FlightClock now? [Y/n] " -n 1 -r
    echo ""
    if [[ ! $REPLY =~ ^[Nn]$ ]]; then
        systemctl restart "$SERVICE_NAME"
        sleep 2
        systemctl status "$SERVICE_NAME" --no-pager || true
    fi
fi
