#!/bin/bash
set -euo pipefail

# Cross-compile FlightClock for Raspberry Pi Zero 2W (aarch64)

TARGET="aarch64-unknown-linux-gnu"
BINARY="target/$TARGET/release/flight-clock"

echo "=== FlightClock Cross-Build ==="
echo "Target: $TARGET"

# Check if cross is installed
if ! command -v cross &> /dev/null; then
    echo "Installing cross..."
    cargo install cross
fi

# Build
echo "Building..."
cross build --target="$TARGET" --release

# Check result
if [ -f "$BINARY" ]; then
    SIZE=$(du -h "$BINARY" | cut -f1)
    echo ""
    echo "=== Build successful ==="
    echo "Binary: $BINARY"
    echo "Size:   $SIZE"
    echo ""
    echo "Deploy to Pi:"
    echo "  scp $BINARY pi@<pi-ip>:/tmp/flight-clock"
    echo "  ssh pi@<pi-ip> 'sudo cp /tmp/flight-clock /opt/flight-clock/ && sudo systemctl restart flight-clock'"
else
    echo "Build failed!"
    exit 1
fi
