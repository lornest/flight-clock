#!/bin/bash
set -euo pipefail

# FlightClock one-line installer.
#   curl -fsSL https://raw.githubusercontent.com/lornest/flight-clock/main/scripts/get.sh | sudo bash
#
# Downloads the latest release, checks its signature against the FlightClock
# release keys, and installs it. Set CHANNEL=beta for pre-releases, or
# HYPERPIXEL=0 to skip the HyperPixel 2.1 Round display setup.

REPO="lornest/flight-clock"
# keys/release.pub and keys/release-backup.pub; either may sign a release
PUBKEYS="RWR2EfaRD2IfiYFaY5C5CBMe4EzVP42dKcLlnI2NM1REP5zxHemgsVfn
RWT0uACmc8JMnclr1ALhzKWopfYRCkPhAi6PUNNj0IYzyAExpxgijqVG"
CHANNEL="${CHANNEL:-stable}"
HYPERPIXEL="${HYPERPIXEL:-1}"

if [ "$(id -u)" -ne 0 ]; then
    echo "Run as root: curl -fsSL https://raw.githubusercontent.com/$REPO/main/scripts/get.sh | sudo bash"
    exit 1
fi
if [ "$(uname -m)" != "aarch64" ]; then
    echo "FlightClock needs 64-bit Raspberry Pi OS (aarch64); this is $(uname -m)."
    exit 1
fi

echo "=== FlightClock ==="
apt-get update -qq
apt-get install -y -qq curl ca-certificates minisign python3 >/dev/null

echo "Finding the latest $CHANNEL release..."
RELEASE=$(curl -fsSL -H "Accept: application/vnd.github+json" "https://api.github.com/repos/$REPO/releases?per_page=30" | CHANNEL="$CHANNEL" python3 -c '
import json, os, sys
beta = os.environ["CHANNEL"] == "beta"
def key(tag):
    core, _, pre = tag.lstrip("v").partition("-")
    return (tuple(int(x) for x in core.split(".")), pre == "", pre)
best = None
for r in json.load(sys.stdin):
    if r["draft"] or (r["prerelease"] and not beta):
        continue
    ver = r["tag_name"].lstrip("v")
    name = "flight-clock-%s-aarch64.tar.gz" % ver
    urls = {a["name"]: a["browser_download_url"] for a in r["assets"]}
    if name in urls and name + ".minisig" in urls:
        if best is None or key(ver) > key(best[0]):
            best = (ver, urls[name], urls[name + ".minisig"])
if best:
    print(" ".join(best))
')
if [ -z "$RELEASE" ]; then
    echo "No signed $CHANNEL release found."
    exit 1
fi
read -r VERSION TARBALL_URL SIG_URL <<< "$RELEASE"
NAME="flight-clock-$VERSION-aarch64"
echo "Installing FlightClock $VERSION"

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
curl -fsSL -o "$WORK/$NAME.tar.gz" "$TARBALL_URL"
curl -fsSL -o "$WORK/$NAME.tar.gz.minisig" "$SIG_URL"

echo "Checking signature..."
OUT=""
for PUBKEY in $PUBKEYS; do
    if OUT=$(minisign -V -P "$PUBKEY" -m "$WORK/$NAME.tar.gz" -x "$WORK/$NAME.tar.gz.minisig" 2>/dev/null); then
        break
    fi
    OUT=""
done
if [ -z "$OUT" ]; then
    echo "Signature check failed. Not installing."
    exit 1
fi
if ! grep -qxF "Trusted comment: flight-clock $VERSION aarch64" <<< "$OUT"; then
    echo "Signature is valid but for a different file. Not installing."
    exit 1
fi

mkdir -p /opt/flight-clock/releases
rm -rf "/opt/flight-clock/releases/$VERSION"
mkdir -p "/opt/flight-clock/releases/$VERSION"
tar -xzf "$WORK/$NAME.tar.gz" -C "/opt/flight-clock/releases/$VERSION" --strip-components=1 --no-same-owner

FLIGHT_CLOCK_NONINTERACTIVE=1 bash "/opt/flight-clock/releases/$VERSION/install.sh"

if [ "$HYPERPIXEL" = "1" ]; then
    bash "/opt/flight-clock/releases/$VERSION/setup-hyperpixel2r.sh"
    echo ""
    echo "Reboot to start the display:  sudo reboot"
fi
