#!/bin/bash
set -euo pipefail

# Build FlightClock for Raspberry Pi Zero 2W (64-bit Pi OS, aarch64).
#
# Builds inside an arm64 Debian Bookworm container. On Apple Silicon this runs
# natively (no emulation); elsewhere Docker emulates arm64 via QEMU (slower).
# Bookworm keeps the binary's glibc requirement at 2.35, so it runs on
# Pi OS Bookworm and newer (not Bullseye).
#
# Output: target/pi/flight-clock

cd "$(dirname "$0")/.."

IMAGE="rust:1-bookworm"
OUT="target/pi/flight-clock"

if ! docker info >/dev/null 2>&1; then
    echo "Error: Docker isn't running."
    exit 1
fi

echo "=== FlightClock Pi build (linux/arm64, $IMAGE) ==="

# Build output lives in a Docker volume: building straight into a macOS
# bind mount intermittently loses files mid-build.
docker volume create flight-clock-cargo >/dev/null
docker volume create flight-clock-target >/dev/null

docker run --rm --platform linux/arm64 \
    -v "$PWD":/src \
    -v flight-clock-cargo:/usr/local/cargo/registry \
    -v flight-clock-target:/build \
    -w /src \
    "$IMAGE" bash -c '
        set -e
        apt-get update -qq
        apt-get install -y -qq pkg-config libudev-dev libinput-dev libxkbcommon-dev \
            libfontconfig-dev libseat-dev libgbm-dev >/dev/null 2>&1
        cargo build --release --target-dir /build
        mkdir -p target/pi
        cp /build/release/flight-clock target/pi/flight-clock
    '

SIZE=$(du -h "$OUT" | cut -f1)
echo ""
echo "=== Build successful ==="
echo "Binary: $OUT ($SIZE)"
echo ""
echo "First install on the Pi:"
echo "  scp $OUT scripts/install.sh <user>@<pi-ip>:/tmp/"
echo "  ssh <user>@<pi-ip> 'cd /tmp && sudo ./install.sh'"
echo ""
echo "Update an existing install:"
echo "  scp $OUT <user>@<pi-ip>:/tmp/flight-clock"
echo "  ssh <user>@<pi-ip> 'sudo cp /tmp/flight-clock /opt/flight-clock/ && sudo systemctl restart flight-clock'"
