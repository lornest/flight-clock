#!/bin/bash
set -euo pipefail

# Sign and publish a draft release built by CI.
#   scripts/publish-release.sh v0.2.0
#
# Downloads the draft's tarball, checks its SHA-256 and its build provenance
# (that CI built it from this tag), signs it with the release key from
# 1Password (never written to disk; or the backup key, with KEY_FILE=), uploads the signature and publishes.
# Clocks only install signed releases, so nothing reaches them until this runs.
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

# The checksum only shows the download is intact. The attestation shows where
# the tarball came from: built by this repo's release workflow, on a GitHub
# runner, from this tag (and from the commit the tag points at locally).
echo "Checking build provenance..."
PROVENANCE=(--repo "$REPO"
    --signer-workflow "$REPO/.github/workflows/release.yml"
    --source-ref "refs/tags/$TAG"
    --deny-self-hosted-runners)
if COMMIT=$(git rev-parse -q --verify "refs/tags/$TAG^{commit}"); then
    PROVENANCE+=(--source-digest "$COMMIT")
else
    echo "  Note: tag $TAG isn't in this checkout, so not checking its commit (git fetch --tags)."
fi
gh attestation verify "$WORK/$NAME" "${PROVENANCE[@]}"

if [ -n "${KEY_FILE:-}" ]; then
    # The backup key: KEY_FILE=/path/to/backup.key scripts/publish-release.sh vX.Y.Z
    echo "Signing with $KEY_FILE..."
    minisign -S -s "$KEY_FILE" -m "$WORK/$NAME" -x "$WORK/$NAME.minisig" -t "flight-clock $VERSION aarch64"
else
    echo "Signing with '$KEY_ITEM' from 1Password..."
    minisign -S \
        -s <(op document get "$KEY_ITEM" --vault "$KEY_VAULT") \
        -m "$WORK/$NAME" \
        -x "$WORK/$NAME.minisig" \
        -t "flight-clock $VERSION aarch64"
fi
# Clocks accept either key
minisign -V -p keys/release.pub -m "$WORK/$NAME" -x "$WORK/$NAME.minisig" 2>/dev/null ||
    minisign -V -p keys/release-backup.pub -m "$WORK/$NAME" -x "$WORK/$NAME.minisig"

echo "Uploading signature and publishing..."
gh release upload "$TAG" --repo "$REPO" "$WORK/$NAME.minisig"
gh release edit "$TAG" --repo "$REPO" --draft=false
echo ""
echo "Published $TAG: https://github.com/$REPO/releases/tag/$TAG"
