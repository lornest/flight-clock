#!/bin/bash
set -euo pipefail

# Package a release tarball (used by CI).
#   scripts/package-release.sh <version> <path-to-aarch64-binary> [out-dir]
# Produces <out-dir>/flight-clock-<version>-aarch64.tar.gz and .sha256

VERSION="${1:?version}"
BINARY="${2:?binary}"
OUT="${3:-dist}"
NAME="flight-clock-$VERSION-aarch64"

cd "$(dirname "$0")/.."
rm -rf "$OUT/$NAME"
mkdir -p "$OUT/$NAME"

install -m755 "$BINARY" "$OUT/$NAME/flight-clock"
echo "$VERSION" > "$OUT/$NAME/VERSION"
install -m755 scripts/install.sh scripts/setup-hyperpixel2r.sh "$OUT/$NAME/"
cp -a systemd migrations keys LICENSE README.md CHANGELOG.md "$OUT/$NAME/"

tar -C "$OUT" --owner=0 --group=0 -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
rm -rf "$OUT/$NAME"
ls -la "$OUT/$NAME.tar.gz"*
