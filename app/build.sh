#!/bin/bash
# Build the menu bar app as a .app bundle.
#
# Usage: ./build.sh [run]   — append `run` to open the built app immediately.
set -euo pipefail

cd "$(dirname "$0")"
CRATE_DIR="$(cd .. && pwd)"
BUILD_DIR="build"
APP_DIR="$BUILD_DIR/PowerMonitorMenuBar.app"

# 1. Build the Rust static lib.
echo ">> cargo build --release -p power-monitor"
(cd "$CRATE_DIR" && cargo build --release)

LIB_DIR="$CRATE_DIR/target/release"
HEADER="$CRATE_DIR/include/power_monitor.h"

# 2. Compile Swift, bridging the C header directly.
#
# Lint knobs turned to 11:
#   -warnings-as-errors          every warning is a build failure
#   -strict-concurrency=complete full Sendable / data-race checking
#   -enable-upcoming-feature *   opt into every Swift-6 upgrade path
mkdir -p "$BUILD_DIR"
echo ">> swiftc PowerMonitorMenuBar.swift"
swiftc -O \
    -target arm64-apple-macos13.0 \
    -parse-as-library \
    -warnings-as-errors \
    -strict-concurrency=complete \
    -enable-upcoming-feature ExistentialAny \
    -enable-upcoming-feature ConciseMagicFile \
    -enable-upcoming-feature ForwardTrailingClosures \
    -enable-upcoming-feature BareSlashRegexLiterals \
    -enable-upcoming-feature ImplicitOpenExistentials \
    -enable-upcoming-feature DisableOutwardActorInference \
    -enable-upcoming-feature InferSendableFromCaptures \
    -enable-upcoming-feature IsolatedDefaultValues \
    -enable-upcoming-feature GlobalConcurrency \
    -import-objc-header "$HEADER" \
    -L "$LIB_DIR" \
    -lpower_monitor \
    -framework IOKit \
    -framework CoreFoundation \
    -lIOReport \
    -o "$BUILD_DIR/PowerMonitorMenuBar" \
    PowerMonitorMenuBar.swift

# 3. Assemble the .app bundle.
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
cp "$BUILD_DIR/PowerMonitorMenuBar" "$APP_DIR/Contents/MacOS/"
cp Info.plist "$APP_DIR/Contents/Info.plist"

echo "Built: $APP_DIR"

if [ "${1:-}" = "run" ]; then
    echo ">> open $APP_DIR"
    open "$APP_DIR"
fi

# 4. Lint-check the standalone gag image script against the same strict knobs.
#    (We don't ship a binary — just want the same warnings-as-errors gate.)
echo ">> swiftc -typecheck render_gag.swift"
swiftc -typecheck \
    -target arm64-apple-macos13.0 \
    -warnings-as-errors \
    -strict-concurrency=complete \
    -enable-upcoming-feature ExistentialAny \
    -enable-upcoming-feature ConciseMagicFile \
    -enable-upcoming-feature ForwardTrailingClosures \
    -enable-upcoming-feature BareSlashRegexLiterals \
    -enable-upcoming-feature ImplicitOpenExistentials \
    -enable-upcoming-feature DisableOutwardActorInference \
    -enable-upcoming-feature InferSendableFromCaptures \
    -enable-upcoming-feature IsolatedDefaultValues \
    -enable-upcoming-feature GlobalConcurrency \
    render_gag.swift
