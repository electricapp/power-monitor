#!/usr/bin/env swift
// Render a gag PNG of `power-monitor` showing an imaginary "Apple M5 Ultra"
// at 1 TB RAM under heavy load. Output → ~/Downloads/m5-ultra-gag.png.
//
// Standalone: no FFI, no Rust library — pure Swift + AppKit with local
// struct + Renderer copies so `swift render_gag.swift` Just Works.

import AppKit
import Foundation

// MARK: - Local value types (mirror the C ABI structs)

struct PmMetrics {
    var sys_power: Float, cpu_power: Float, gpu_power: Float, ane_power: Float, dram_power: Float
    var cpu_temp: Float, gpu_temp: Float
    var pcpu_util: Float, pcpu_mhz: UInt32
    var ecpu_util: Float, ecpu_mhz: UInt32
    var gpu_util: Float, gpu_mhz: UInt32
    var mem_used: UInt64, mem_total: UInt64
    var swap_used: UInt64, swap_total: UInt64
    var fan_rpm: UInt32, fan_max_rpm: UInt32
    var interval_ms: Float
}

struct PmSocInfo {
    var pcpu_cores: UInt32
    var ecpu_cores: UInt32
    var gpu_cores: UInt32
    var total_ram: UInt64
}

// MARK: - Fake data (M5 Ultra extrapolation under heavy load)

let soc = PmSocInfo(
    pcpu_cores: 32,           // 2× M4 Ultra perf cores
    ecpu_cores: 8,
    gpu_cores: 80,            // M4 Ultra has 76; round-up extrapolation
    total_ram: UInt64(1024) * (1 << 30)   // 1 TB
)

let metrics = PmMetrics(
    sys_power: 268.4,
    cpu_power: 78.9,
    gpu_power: 142.1,
    ane_power: 6.3,
    dram_power: 14.7,
    cpu_temp: 94.0,           // red
    gpu_temp: 88.0,           // red
    pcpu_util: 0.98, pcpu_mhz: 5200,
    ecpu_util: 0.84, ecpu_mhz: 3100,
    gpu_util:  0.96, gpu_mhz:  2100,
    mem_used:  UInt64(964) * (1 << 30),    // 964 GB of 1 TB
    mem_total: UInt64(1024) * (1 << 30),
    swap_used: UInt64(12)  * (1 << 30),
    swap_total: UInt64(16) * (1 << 30),
    fan_rpm: 4800, fan_max_rpm: 5800,
    interval_ms: 1002.0
)

let chip = "Apple M5 Ultra"
let host = "cosmos"

// MARK: - Renderer (mirror of app/PowerMonitorMenuBar.swift)

enum Renderer {
    // Widen slightly vs. the live dashboard (56) so 3-digit watts and a
    // 1 TB memory row don't shove the right border out of alignment.
    static let INNER = 60
    static let BAR_W = 24

    // Version — read Cargo.toml at runtime so this script can't drift
    // from the crate it purports to render. Same source of truth that
    // `pm_version()` pulls from via `env!("CARGO_PKG_VERSION")`.
    static let version: String = {
        let cargoPath = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()   // app/
            .deletingLastPathComponent()   // power-monitor/
            .appendingPathComponent("Cargo.toml")
        guard let contents = try? String(contentsOf: cargoPath, encoding: .utf8) else { return "?" }
        for raw in contents.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
            // Match only `version = "..."` at the package level (first hit).
            if line.hasPrefix("version") {
                let parts = line.split(separator: "\"")
                if parts.count >= 2 { return String(parts[1]) }
            }
        }
        return "?"
    }()

    static let green  = NSColor(red: 0.30, green: 0.78, blue: 0.34, alpha: 1)
    static let yellow = NSColor(red: 0.92, green: 0.77, blue: 0.24, alpha: 1)
    static let red    = NSColor(red: 0.93, green: 0.35, blue: 0.35, alpha: 1)
    static let cyan   = NSColor(red: 0.38, green: 0.82, blue: 0.82, alpha: 1)
    static let dim    = NSColor(white: 0.48, alpha: 1)
    static let fg     = NSColor(white: 0.92, alpha: 1)

    // Menlo has a complete box-drawing block at consistent monospace widths;
    // `.monospacedSystemFont` (SF Mono) falls back to proportional glyphs for
    // `│ ─ ╭ ╮ ╰ ╯`, which breaks the grid.
    // NSFont isn't Sendable; script is single-threaded so the marker is safe.
    nonisolated(unsafe) static let mono     = NSFont(name: "Menlo", size: 13)!
    nonisolated(unsafe) static let monoBold = NSFont(name: "Menlo-Bold", size: 13)!

    static func heat(_ frac: Float) -> NSColor {
        frac < 0.40 ? green : frac < 0.75 ? yellow : red
    }

    static func tempColor(_ t: Float) -> NSColor {
        t < 70 ? green : t < 85 ? yellow : red
    }

    static func tint(_ s: String, _ c: NSColor, bold: Bool = false) -> NSAttributedString {
        NSAttributedString(string: s, attributes: [
            .foregroundColor: c,
            .font: bold ? monoBold : mono,
        ])
    }

    static func bar(_ val: Float, max maxV: Float) -> NSAttributedString {
        let frac = min(Swift.max(maxV == 0 ? 0 : val / maxV, 0), 1)
        let n = Int((frac * Float(BAR_W)).rounded())
        let out = NSMutableAttributedString()
        out.append(tint(String(repeating: "█", count: n), heat(frac)))
        out.append(tint(String(repeating: "░", count: BAR_W - n), dim))
        return out
    }

    static func leftCol(_ label: String, pct: Float) -> NSAttributedString {
        let p = min(Swift.max(pct, 0), 100)
        let out = NSMutableAttributedString()
        out.append(tint("  ", dim))
        out.append(tint(label.padding(toLength: 5, withPad: " ", startingAt: 0), fg, bold: true))
        out.append(tint(" ", dim))
        out.append(tint(String(format: "%3.0f%%", p), dim))
        out.append(tint("  ", dim))
        return out
    }

    static func powerRow(_ label: String, _ w: Float, max maxW: Float) -> NSAttributedString {
        let out = NSMutableAttributedString(attributedString: leftCol(label, pct: w / maxW * 100))
        out.append(bar(w, max: maxW))
        out.append(tint(String(format: "  %5.2f  W", w), cyan))
        return out
    }

    static func fanRow(_ rpm: UInt32, maxRpm: UInt32) -> NSAttributedString {
        if maxRpm == 0 {
            let out = NSMutableAttributedString()
            out.append(tint("  ", dim))
            out.append(tint("FAN", fg, bold: true))
            out.append(tint("    —  \(String(repeating: "─", count: BAR_W))  fanless", dim))
            return out
        }
        let duty = min(Swift.max(Float(rpm) / Float(maxRpm), 0), 1)
        let out = NSMutableAttributedString(attributedString: leftCol("FAN", pct: duty * 100))
        out.append(bar(duty, max: 1))
        out.append(tint(String(format: "  %4u RPM", rpm), cyan))
        return out
    }

    static func powerRowTemp(_ label: String, _ w: Float, max maxW: Float, temp: Float) -> NSAttributedString {
        let out = NSMutableAttributedString(attributedString: powerRow(label, w, max: maxW))
        out.append(tint("  (", dim))
        out.append(tint(String(format: "%.0f°C", temp), tempColor(temp)))
        out.append(tint(")", dim))
        return out
    }

    static func utilRow(_ label: String, _ util: Float, _ mhz: UInt32) -> NSAttributedString {
        let out = NSMutableAttributedString(attributedString: leftCol(label, pct: util * 100))
        out.append(bar(util, max: 1))
        out.append(tint(String(format: "  %4u MHz", mhz), cyan))
        return out
    }

    static func memRow(_ label: String, used: UInt64, total: UInt64) -> NSAttributedString {
        let pct = total > 0 ? Float(used) / Float(total) : 0
        let out = NSMutableAttributedString(attributedString: leftCol(label, pct: pct * 100))
        out.append(bar(pct, max: 1))
        let usedGb = Double(used) / Double(1 << 30)
        let totalGb = Double(total) / Double(1 << 30)
        out.append(tint(String(format: "  %5.1f", usedGb), cyan))
        out.append(tint(String(format: " / %5.1f GB", totalGb), dim))
        return out
    }

    static func visualWidth(_ s: NSAttributedString) -> Int {
        s.string.count
    }

    static func boxRow(_ content: NSAttributedString) -> NSAttributedString {
        let pad = Swift.max(INNER - visualWidth(content), 0)
        let out = NSMutableAttributedString()
        out.append(tint("│", dim))
        out.append(content)
        out.append(tint(String(repeating: " ", count: pad), dim))
        out.append(tint("│\n", dim))
        return out
    }

    static func boxEmpty() -> NSAttributedString {
        tint("│\(String(repeating: " ", count: INNER))│\n", dim)
    }

    static func boxRule() -> NSAttributedString {
        tint("│\(String(repeating: "─", count: INNER))│\n", dim)
    }

    static func timeStamp() -> String {
        let m = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"]
        let c = Calendar.current.dateComponents([.hour, .minute, .day, .month], from: Date())
        let mon = m[(c.month ?? 1) - 1]
        return String(format: "%02d:%02d %02d-\(mon)", c.hour ?? 0, c.minute ?? 0, c.day ?? 0)
    }

    static func frame(m: PmMetrics, soc: PmSocInfo, chip: String, host: String) -> NSAttributedString {
        let totalGb = Int((Double(soc.total_ram) / Double(1 << 30)).rounded())
        let title = soc.gpu_cores > 0
            ? " v\(version) · \(chip) · \(soc.pcpu_cores)P + \(soc.ecpu_cores)E · \(soc.gpu_cores) GPU · \(totalGb)GB "
            : " v\(version) · \(chip) · \(soc.pcpu_cores)P + \(soc.ecpu_cores)E · \(totalGb)GB "

        let out = NSMutableAttributedString()
        let topPad = Swift.max(INNER + 2 - title.count - 3, 0)
        out.append(tint("╭─", dim))
        out.append(tint(title, fg, bold: true))
        out.append(tint(String(repeating: "─", count: topPad) + "╮\n", dim))

        out.append(boxEmpty())
        out.append(boxRow(powerRow("SYS", m.sys_power, max: 400)))
        out.append(boxRow(fanRow(m.fan_rpm, maxRpm: m.fan_max_rpm)))
        out.append(boxRule())
        out.append(boxRow(powerRowTemp("GPU", m.gpu_power, max: 160, temp: m.gpu_temp)))
        out.append(boxRow(powerRowTemp("CPU", m.cpu_power, max: 100, temp: m.cpu_temp)))
        out.append(boxRow(powerRow("ANE", m.ane_power, max: 10)))
        out.append(boxRow(powerRow("DRAM", m.dram_power, max: 20)))

        out.append(boxEmpty())
        out.append(boxRow(utilRow("PCPU", m.pcpu_util, m.pcpu_mhz)))
        out.append(boxRow(utilRow("ECPU", m.ecpu_util, m.ecpu_mhz)))
        out.append(boxRow(utilRow("GPU",  m.gpu_util,  m.gpu_mhz)))

        out.append(boxEmpty())
        out.append(boxRow(memRow("MEM",  used: m.mem_used,  total: m.mem_total)))
        out.append(boxRow(memRow("SWAP", used: m.swap_used, total: m.swap_total)))

        out.append(boxEmpty())

        let intervalStr = String(format: " %.0f ms ", m.interval_ms)
        let sysStr = " \"\(host)\" \(timeStamp()) "
        let bpad = Swift.max(INNER + 2 - sysStr.count - intervalStr.count - 2, 0)
        out.append(tint("╰\(sysStr)\(String(repeating: "─", count: bpad))\(intervalStr)╯", dim))

        return out
    }
}

// MARK: - Attributed string → PNG via NSTextView

let ns = Renderer.frame(m: metrics, soc: soc, chip: chip, host: host)

let padding: CGFloat = 28
let measured = ns.boundingRect(
    with: NSSize(width: 2000, height: 4000),
    options: [.usesLineFragmentOrigin]
)
let imgSize = NSSize(
    width: ceil(measured.width) + padding * 2,
    height: ceil(measured.height) + padding * 2
)

let textView = NSTextView(frame: NSRect(origin: .zero, size: imgSize))
textView.isEditable = false
textView.isSelectable = false
textView.drawsBackground = true
textView.backgroundColor = NSColor(white: 0.07, alpha: 1)
textView.textContainerInset = NSSize(width: padding, height: padding)

// Critical: NSTextContainer defaults to lineFragmentPadding=5 and a fixed
// width that tracks the view. Both cause mid-line wrapping that eats the
// right-side box border and corner glyphs. Disable both.
if let container = textView.textContainer {
    container.lineFragmentPadding = 0
    container.widthTracksTextView = false
    container.heightTracksTextView = false
    container.size = NSSize(
        width: CGFloat.greatestFiniteMagnitude,
        height: CGFloat.greatestFiniteMagnitude
    )
}

textView.textStorage?.setAttributedString(ns)

// Force layout so the image rep captures the text.
if let container = textView.textContainer, let lm = textView.layoutManager {
    lm.ensureLayout(for: container)
}

guard let rep = textView.bitmapImageRepForCachingDisplay(in: textView.bounds) else {
    fputs("error: could not build bitmap rep\n", stderr)
    exit(1)
}
textView.cacheDisplay(in: textView.bounds, to: rep)

guard let png = rep.representation(using: .png, properties: [:]) else {
    fputs("error: could not encode PNG\n", stderr)
    exit(1)
}

let out = (NSHomeDirectory() as NSString).appendingPathComponent("Downloads/m5-ultra-gag.png")
do {
    try png.write(to: URL(fileURLWithPath: out))
    print("Wrote: \(out)")
    print("Size:  \(Int(imgSize.width))x\(Int(imgSize.height))")
} catch {
    fputs("error: \(error)\n", stderr)
    exit(1)
}
