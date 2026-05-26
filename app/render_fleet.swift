#!/usr/bin/env swift
// Render a fleet meme image: 24 cosmo-N tiles arranged 4 rows × 6 cols.
// Each tile is the *full* single-view dashboard (box border, title, all rows
// and footer), not an abbreviated layout — same renderer as `render_gag.swift`.
//
// Output → ~/Downloads/cosmo-fleet-24.png

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

// MARK: - Deterministic pseudo-random

// Tiny hash-based PRNG, good enough for meme-scale variety.
func prng(_ seed: Int, _ slot: Int) -> Float {
    var x = UInt32(truncatingIfNeeded: seed &* 2654435761 &+ slot &* 40503)
    x ^= x >> 13
    x &*= 0x5bd1e995
    x ^= x >> 15
    return Float(x & 0x00FF_FFFF) / Float(0x0100_0000)
}

func makeMetrics(index: Int) -> PmMetrics {
    PmMetrics(
        sys_power:  210 + prng(index, 1) * 80,          // 210–290 W
        cpu_power:  62  + prng(index, 2) * 34,          // 62–96 W
        gpu_power:  115 + prng(index, 3) * 42,          // 115–157 W
        ane_power:  4   + prng(index, 4) * 5,           // 4–9 W
        dram_power: 10  + prng(index, 5) * 8,           // 10–18 W
        cpu_temp:   82  + prng(index, 6) * 13,          // 82–95 °C
        gpu_temp:   80  + prng(index, 7) * 13,          // 80–93 °C
        pcpu_util:  0.78 + prng(index, 8) * 0.21,
        pcpu_mhz:   4600 + UInt32(prng(index, 9) * 900),
        ecpu_util:  0.68 + prng(index, 10) * 0.27,
        ecpu_mhz:   2800 + UInt32(prng(index, 11) * 600),
        gpu_util:   0.84 + prng(index, 12) * 0.15,
        gpu_mhz:    1900 + UInt32(prng(index, 13) * 300),
        mem_used:   UInt64(780 + prng(index, 14) * 230) * (1 << 30),
        mem_total:  UInt64(1024) * (1 << 30),
        swap_used:  UInt64(2  + prng(index, 15) * 12) * (1 << 30),
        swap_total: UInt64(16) * (1 << 30),
        fan_rpm: 4500 + UInt32(prng(index, 17) * 1200),
        fan_max_rpm: 5800,
        interval_ms: 980 + prng(index, 16) * 40
    )
}

let soc = PmSocInfo(pcpu_cores: 32, ecpu_cores: 8, gpu_cores: 80,
                    total_ram: UInt64(1024) * (1 << 30))
let chip = "Apple M5 Ultra"

// MARK: - Renderer (identical to render_gag.swift — single source of layout)

enum Renderer {
    static let INNER = 60
    static let BAR_W = 24

    // Version — read Cargo.toml at runtime; same source of truth as `pm_version()`.
    static let version: String = {
        let cargoPath = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("Cargo.toml")
        guard let contents = try? String(contentsOf: cargoPath, encoding: .utf8) else { return "?" }
        for raw in contents.split(separator: "\n") {
            let line = raw.trimmingCharacters(in: .whitespaces)
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

    // Menlo — complete box-drawing block at consistent monospace widths.
    nonisolated(unsafe) static let mono     = NSFont(name: "Menlo", size: 11)!
    nonisolated(unsafe) static let monoBold = NSFont(name: "Menlo-Bold", size: 11)!

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

    static func visualWidth(_ s: NSAttributedString) -> Int { s.string.count }

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
        out.append(boxRow(powerRow("SYS", m.sys_power, max: 300)))
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

// MARK: - Grid assembly

let COLS = 3
let ROWS = 2
let COUNT = COLS * ROWS  // 6

let tiles: [NSAttributedString] = (0..<COUNT).map { i in
    Renderer.frame(
        m: makeMetrics(index: i),
        soc: soc,
        chip: chip,
        host: "cosmo-\(i)"
    )
}

// Measure max tile size (every tile is the same shape; one measurement is fine).
let probe = tiles[0].boundingRect(
    with: NSSize(width: CGFloat.greatestFiniteMagnitude,
                 height: CGFloat.greatestFiniteMagnitude),
    options: [.usesLineFragmentOrigin]
)
let tileW = ceil(probe.width)
let tileH = ceil(probe.height)

let gapX: CGFloat = 22
let gapY: CGFloat = 18
let outerPad: CGFloat = 32
let imgW = outerPad * 2 + CGFloat(COLS) * tileW + CGFloat(COLS - 1) * gapX
let imgH = outerPad * 2 + CGFloat(ROWS) * tileH + CGFloat(ROWS - 1) * gapY
let imgSize = NSSize(width: imgW, height: imgH)

// MARK: - Draw into a 2× bitmap rep

let scale: CGFloat = 2
guard let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil,
    pixelsWide: Int(imgW * scale),
    pixelsHigh: Int(imgH * scale),
    bitsPerSample: 8,
    samplesPerPixel: 4,
    hasAlpha: true,
    isPlanar: false,
    colorSpaceName: .deviceRGB,
    bytesPerRow: 0,
    bitsPerPixel: 32
) else {
    fputs("error: could not create bitmap rep\n", stderr)
    exit(1)
}
rep.size = imgSize

guard let ctx = NSGraphicsContext(bitmapImageRep: rep) else {
    fputs("error: could not build graphics context\n", stderr)
    exit(1)
}
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = ctx

// Non-flipped context. `.usesLineFragmentOrigin` anchors at rect top (y+height);
// we position each tile so row 0 sits at the top of the image.

NSColor(white: 0.06, alpha: 1).setFill()
NSRect(origin: .zero, size: imgSize).fill()

for (i, tile) in tiles.enumerated() {
    let col = i % COLS
    let row = i / COLS
    let x = outerPad + CGFloat(col) * (tileW + gapX)
    let y = imgH - outerPad - tileH - CGFloat(row) * (tileH + gapY)
    tile.draw(
        with: NSRect(x: x, y: y, width: tileW, height: tileH),
        options: [.usesLineFragmentOrigin]
    )
}

NSGraphicsContext.restoreGraphicsState()

// MARK: - Encode PNG

guard let png = rep.representation(using: .png, properties: [:]) else {
    fputs("error: could not encode PNG\n", stderr)
    exit(1)
}

let outPath = (NSHomeDirectory() as NSString)
    .appendingPathComponent("Downloads/cosmo-fleet-6.png")
do {
    try png.write(to: URL(fileURLWithPath: outPath))
    print("Wrote: \(outPath)")
    print("Grid:  \(COLS) × \(ROWS) = \(COUNT) full-view tiles")
    print("Size:  \(Int(imgW))×\(Int(imgH)) pt  (\(Int(imgW * scale))×\(Int(imgH * scale)) px @ 2×)")
} catch {
    fputs("error: \(error)\n", stderr)
    exit(1)
}
