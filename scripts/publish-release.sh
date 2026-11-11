#!/bin/bash
set -euo pipefail

# Sign and publish a draft release built by CI.
#   scripts/publish-release.sh v0.2.0
#
# Downloads the draft's tarball, checks its SHA-256, signs it with the release
# key from 1Password (never written to disk), uploads the signature and
# publishes. Clocks only install signed releases, so nothing reaches them
# until this runs.
#
# Needs: gh (logged in), op (1Password CLI, signed in), minisign.

TAG="${1:?usage: publish-release.sh vX.Y.Z}"
VERSION="${TAG#v}"
NAME="flight-clock-$VERSION-aarch64.tar.gz"
REPO="lornest/flight-clock"
KEY_ITEM="flight-clock release signing key"
KEY_VAULT="Private"

cd "$(dirname "$0")/.."
for cmd in gh op minisign; do
    command -v "$cmd" >/dev/null || { echo "Error: $cmd not installed"; exit 1; }
done

IS_DRAFT=$(gh release view "$TAG" --repo "$REPO" --json isDraft --jq .isDraft)
if [ "$IS_DRAFT" != "true" ]; then
    echo "Error: $TAG isn't a draft release (already published?)"
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "Downloading $NAME..."
gh release download "$TAG" --repo "$REPO" --pattern "$NAME" --pattern "$NAME.sha256" --dir "$WORK"
(cd "$WORK" && shasum -a 256 -c "$NAME.sha256")

echo "Signing with '$KEY_ITEM' from 1Password..."
minisign -S \
    -s <(op document get "$KEY_ITEM" --vault "$KEY_VAULT") \
    -m "$WORK/$NAME" \
    -x "$WORK/$NAME.minisig" \
    -t "flight-clock $VERSION aarch64"
minisign -V -p keys/release.pub -m "$WORK/$NAME" -x "$WORK/$NAME.minisig"

echo "Uploading signature and publishing..."
gh release upload "$TAG" --repo "$REPO" "$WORK/$NAME.minisig"
gh release edit "$TAG" --repo "$REPO" --draft=false
echo ""
echo "Published $TAG: https://github.com/$REPO/releases/tag/$TAG"
