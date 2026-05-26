#!/bin/bash
# Codesign + notarize the menu bar app for distribution without the Gatekeeper
# quarantine song-and-dance. SCAFFOLD ONLY — requires a paid Apple Developer
# account and a stored notarytool keychain profile. Will fail fast if unset.
#
# One-time setup (each machine):
#   1. Enroll in Apple Developer Program ($99/yr)
#   2. Xcode → Settings → Accounts → `+` → Developer ID Application cert
#   3. Create app-specific password at appleid.apple.com
#   4. xcrun notarytool store-credentials notary-profile \
#          --apple-id YOU@example.com \
#          --team-id  YOURTEAMID \
#          --password <app-specific-password>
#   5. Export the signing identity name:
#          export CODESIGN_IDENTITY="Developer ID Application: ORG (TEAMID)"
#          export NOTARY_PROFILE="notary-profile"
set -euo pipefail

cd "$(dirname "$0")"

: "${CODESIGN_IDENTITY:?set CODESIGN_IDENTITY to your Developer ID Application identity string}"
: "${NOTARY_PROFILE:=notary-profile}"

CRATE_DIR="$(cd .. && pwd)"
VERSION=$(grep -E '^version = ' "$CRATE_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
BUILD_DIR="build"
APP_DIR="$BUILD_DIR/PowerMonitorMenuBar.app"
ZIP_NAME="PowerMonitorMenuBar-v${VERSION}-macos-arm64.zip"
ZIP_PATH="$BUILD_DIR/$ZIP_NAME"

echo "=== Notarize PowerMonitorMenuBar v$VERSION ==="
echo "    identity: $CODESIGN_IDENTITY"
echo "    profile:  $NOTARY_PROFILE"

# 1. Build (non-release script builds the unsigned binary).
./build.sh

# 2. Codesign. `--options runtime` enables hardened runtime — required for
#    notarization. `--deep` recursively signs any embedded binaries.
codesign --sign "$CODESIGN_IDENTITY" \
         --options runtime \
         --deep --force \
         --timestamp \
         "$APP_DIR"

codesign --verify --strict --verbose=2 "$APP_DIR"

# 3. Zip for submission.
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_DIR" "$ZIP_PATH"

# 4. Submit to Apple's notary service, wait, and staple the ticket.
xcrun notarytool submit "$ZIP_PATH" \
    --keychain-profile "$NOTARY_PROFILE" \
    --wait

xcrun stapler staple "$APP_DIR"
xcrun stapler validate "$APP_DIR"

# 5. Re-zip the now-stapled bundle so downloaders get the ticket.
rm -f "$ZIP_PATH"
ditto -c -k --keepParent "$APP_DIR" "$ZIP_PATH"

SHA=$(shasum -a 256 "$ZIP_PATH" | awk '{print $1}')
echo
echo "=== signed + notarized ==="
echo "  file:   $(pwd)/$ZIP_PATH"
echo "  sha256: $SHA"
echo
echo "Update homebrew/power-monitor.rb with this sha256 before submitting the cask PR."
