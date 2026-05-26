#!/bin/bash
# Build the menu bar app and package it for GitHub release.
#
# Produces:
#   build/PowerMonitorMenuBar.app           — the unsigned bundle
#   build/PowerMonitorMenuBar-v<VER>.zip    — release artifact to upload
#
# Unsigned bundles trip Gatekeeper on first launch; the release notes
# template at release-notes.md documents the quarantine-removal one-liner
# for downloaders.
#
# To upgrade to a signed + notarized build later, see notarize.sh.
set -euo pipefail

cd "$(dirname "$0")"
CRATE_DIR="$(cd .. && pwd)"
BUILD_DIR="build"
APP_DIR="$BUILD_DIR/PowerMonitorMenuBar.app"

# Read version from the crate — single source of truth.
VERSION=$(grep -E '^version = ' "$CRATE_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
ZIP_NAME="PowerMonitorMenuBar-v${VERSION}-macos-arm64.zip"
ZIP_PATH="$BUILD_DIR/$ZIP_NAME"

echo "=== PowerMonitorMenuBar v$VERSION release build ==="

# 1. Build the .app (calls cargo build --release under the hood).
./build.sh

# 2. Drop any extended attributes that might stow dev-machine metadata.
xattr -cr "$APP_DIR"

# 3. Zip with ditto so the bundle preserves symlinks + resource forks correctly.
#    ditto is the canonical tool for packaging .app bundles on macOS.
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_DIR" "$ZIP_PATH"

BYTES=$(stat -f%z "$ZIP_PATH")
HR=$(awk "BEGIN { printf \"%.2f\", $BYTES/1024/1024 }")
SHA=$(shasum -a 256 "$ZIP_PATH" | awk '{print $1}')

echo
cat <<EOF
=== release artifact ready ===

  file:   $(pwd)/$ZIP_PATH
  size:   ${HR} MB (${BYTES} bytes)
  sha256: $SHA

Next (requires GitHub account, no Apple Developer needed):
  gh release create v${VERSION} \\
      --title "v${VERSION}" \\
      --notes-file release-notes.md \\
      "$ZIP_PATH"

Downloaders run one command to clear Gatekeeper's quarantine bit
(documented in release-notes.md):
  xattr -dr com.apple.quarantine ~/Downloads/PowerMonitorMenuBar.app
EOF
