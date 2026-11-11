#!/bin/bash
set -euo pipefail

# HyperPixel 2.1 Round display + touch setup for FlightClock.
# Run as root on the Pi (Raspberry Pi OS Bookworm/Trixie), then reboot.
#
# Why not the kernel's vc4-kms-dpi-hyperpixel2r overlay? Its panel driver
# (st7701, bit-banged SPI) keeps GPIO 10/11 claimed while the screen is on,
# and the touch controller's I2C bus needs the same pins, so touch never
# appears. Instead this uses Pimoroni's legacy setup:
#   - firmware drives the DPI display, the app draws to /dev/fb0 (no KMS)
#   - Pimoroni's one-shot script initialises the panel over GPIO, then exits
#   - Pimoroni's overlay adds the touch controller (edt-ft5x06 @ 0x15)
#
# It also hides boot text on the display (optional, see QUIET_BOOT).
#
# Re-running is safe. Backups: config.txt.bak-flightclock-display and
# cmdline.txt.bak-flightclock-display in /boot/firmware (first run only).

# Pimoroni's repo, pinned to the reviewed commit
PIMORONI_REPO="pimoroni/hyperpixel2r"
PIMORONI_COMMIT="2af6cafbdaf4a8e44509ae45517a373f488adc02"

QUIET_BOOT="${QUIET_BOOT:-1}"   # QUIET_BOOT=0 ./setup-hyperpixel2r.sh to keep boot text

BOOT=/boot/firmware
CONFIG="$BOOT/config.txt"
CMDLINE="$BOOT/cmdline.txt"

if [ "$(id -u)" -ne 0 ]; then
    echo "Error: run as root (sudo)"
    exit 1
fi
if [ ! -f "$CONFIG" ] || [ ! -f "$CMDLINE" ]; then
    echo "Error: $CONFIG or $CMDLINE not found (expects Raspberry Pi OS Bookworm or later)"
    exit 1
fi

echo "=== HyperPixel 2.1 Round setup ==="

# --- 1. Packages ---
echo "[1/6] Installing packages..."
apt-get update -qq
# python3-rpi.gpio (classic RPi.GPIO) replaces python3-rpi-lgpio: lgpio goes
# through the kernel GPIO API and refuses pins the touch I2C bus has claimed.
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq device-tree-compiler python3-rpi.gpio curl >/dev/null

# --- 2. Pimoroni overlay + panel init script ---
echo "[2/6] Installing Pimoroni overlay and panel init (commit ${PIMORONI_COMMIT:0:7})..."
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
curl -fsSL "https://github.com/$PIMORONI_REPO/archive/$PIMORONI_COMMIT.tar.gz" | tar xz -C "$WORK" --strip-components=1
dtc -@ -I dts -O dtb -o "$WORK/hyperpixel2r.dtbo" "$WORK/src/hyperpixel2r-overlay.dts" 2>/dev/null
install -m644 "$WORK/hyperpixel2r.dtbo" "$BOOT/overlays/hyperpixel2r.dtbo"
install -m755 "$WORK/dist/hyperpixel2r-init" /usr/local/bin/hyperpixel2r-init

# --- 3. Services ---
echo "[3/6] Installing services..."
# Pimoroni's own unit runs `python`, which doesn't exist on current Pi OS.
cat > /etc/systemd/system/hyperpixel2r-init.service <<'EOF'
[Unit]
Description=HyperPixel 2.1 Round panel init (bit-banged SPI on GPIO 10/11/18)
DefaultDependencies=no
After=basic.target
Before=flight-clock.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/bin/python3 /usr/local/bin/hyperpixel2r-init

[Install]
WantedBy=multi-user.target
EOF

mkdir -p /etc/systemd/system/flight-clock.service.d
cat > /etc/systemd/system/flight-clock.service.d/hyperpixel2r.conf <<'EOF'
# Legacy firmware DPI: panel is initialised by hyperpixel2r-init and the app
# draws to /dev/fb0 (no KMS), which leaves GPIO 10/11 free for touch.
[Unit]
After=hyperpixel2r-init.service
Wants=hyperpixel2r-init.service

[Service]
Environment=SLINT_BACKEND_LINUXFB=1
EOF

systemctl daemon-reload
systemctl enable hyperpixel2r-init.service >/dev/null 2>&1

# --- 4. config.txt ---
echo "[4/6] Updating $CONFIG..."
[ -f "$CONFIG.bak-flightclock-display" ] || cp "$CONFIG" "$CONFIG.bak-flightclock-display"

# KMS must be off for the firmware-driven DPI display
sed -i -e 's/^dtoverlay=vc4-kms-v3d/#dtoverlay=vc4-kms-v3d/' \
       -e 's/^dtoverlay=vc4-fkms-v3d/#dtoverlay=vc4-fkms-v3d/' \
       -e 's/^dtoverlay=vc4-kms-dpi-hyperpixel2r/#dtoverlay=vc4-kms-dpi-hyperpixel2r/' "$CONFIG"

if ! grep -q '^dtoverlay=hyperpixel2r$' "$CONFIG"; then
    # Must apply to all models: make sure we're in an [all] section
    cat >> "$CONFIG" <<'EOF'

[all]
# HyperPixel 2.1 Round, legacy firmware DPI (touch needs GPIO 10/11 free)
dtoverlay=hyperpixel2r
enable_dpi_lcd=1
dpi_group=2
dpi_mode=87
dpi_output_format=0x7f216
dpi_timings=480 0 10 16 55 480 0 15 60 15 0 0 0 60 0 19200000 6
EOF
fi

# --- 5. Quiet boot ---
if [ "$QUIET_BOOT" = "1" ]; then
    echo "[5/6] Hiding boot text on the display..."
    [ -f "$CMDLINE.bak-flightclock-display" ] || cp "$CMDLINE" "$CMDLINE.bak-flightclock-display"

    # No rainbow splash from the firmware
    grep -q '^disable_splash=1' "$CONFIG" || printf '\n# Hide the firmware rainbow splash\ndisable_splash=1\n' >> "$CONFIG"

    # cmdline.txt is a single line: edit it token by token
    line=$(tr -d '\n' < "$CMDLINE")
    # Kernel messages go to tty3 (not shown) instead of tty1 (the display)
    line=$(echo "$line" | sed -E 's/(^| )console=tty1( |$)/\1console=tty3\2/')
    for token in quiet loglevel=3 logo.nologo vt.global_cursor_default=0 consoleblank=0; do
        key=${token%%=*}
        if ! echo " $line " | grep -qE " ${key}(=[^ ]*)? "; then
            line="$line $token"
        fi
    done
    printf '%s\n' "$line" > "$CMDLINE"

    # No login prompt on the display (SSH is unaffected)
    systemctl disable getty@tty1.service >/dev/null 2>&1 || true
else
    echo "[5/6] Leaving boot text visible (QUIET_BOOT=0)"
fi

# --- 6. Done ---
echo "[6/6] Done."
echo ""
echo "  Reboot to apply:  sudo reboot"
echo "  If the screen stays black after a warm reboot, power-cycle the Pi"
echo "  (unplug for ~10s) - the panel sometimes needs a cold start."
echo ""
echo "  Undo: restore $CONFIG.bak-flightclock-display and"
echo "        $CMDLINE.bak-flightclock-display, then reboot."
