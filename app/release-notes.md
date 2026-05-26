# PowerMonitorMenuBar v0.1.0

Apple Silicon power and performance menu bar app. Mirrors the terminal
dashboard pixel-for-pixel; FFI into the `power-monitor` Rust crate — no
subprocess, no `sudo`, no external daemons.

## Install

1. Download `PowerMonitorMenuBar-v0.1.0-macos-arm64.zip` below, unzip.
2. Move `PowerMonitorMenuBar.app` into `/Applications` (or anywhere).
3. **Clear Gatekeeper's quarantine bit** (this build is unsigned):

   ```bash
   xattr -dr com.apple.quarantine /Applications/PowerMonitorMenuBar.app
   ```

4. Double-click to launch. A bolt icon appears in the menu bar.

Left-click → dashboard popover. Right-click → Quit. Any click outside the
popover dismisses it.

## Requirements

- Apple Silicon (arm64) Mac
- macOS 13 Ventura or later

## What's inside

- FFI to `libpower_monitor.a` (Rust static lib, no runtime deps)
- Pure SwiftUI + AppKit; no third-party frameworks
- `-warnings-as-errors -strict-concurrency=complete` clean build

## Known limitations

- Unsigned — downloaders need the `xattr` command above.
- Not yet on Homebrew; planned after signed release.
