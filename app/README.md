# PowerMonitor menu bar app

A SwiftUI `MenuBarExtra` that mirrors `power-monitor`'s terminal dashboard
glyph-for-glyph. Calls into `libpower_monitor.a` directly — no subprocess.

## Build & run

```
./build.sh run
```

That script:

1. `cargo build --release` of the crate → produces `libpower_monitor.a`
2. `swiftc` of `PowerMonitorMenuBar.swift`, bridging `include/power_monitor.h`
   directly with `-import-objc-header`
3. Assembles `build/PowerMonitorMenuBar.app` with an `LSUIElement` Info.plist
   (no Dock icon, menu bar only)
4. `open`s the bundle

The Rust side exposes five FFI functions (`pm_sampler_{new,free,sample,
soc_info,chip_name}` — see `src/ffi.rs`). The Swift side blocks on
`pm_sampler_sample(h, 1000, &m)` from a `Task.detached` loop, then renders
an `AttributedString` with the same box-drawing characters, bar glyphs
(`█░`), and ANSI-equivalent heat colors as the TUI.

## Files

| File                        | What it does                                          |
| --------------------------- | ----------------------------------------------------- |
| `PowerMonitorMenuBar.swift` | App + popover view + Renderer (port of `src/main.rs`) |
| `Info.plist`                | Menu bar only (`LSUIElement`); macOS 13+              |
| `build.sh`                  | Rust staticlib → swiftc bridging header → .app bundle |
