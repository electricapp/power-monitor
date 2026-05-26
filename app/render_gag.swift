#!/usr/bin/env swift
// Render an animated GIF of `power-monitor` showing an imaginary "Apple M5 Ultra"
// at 1 TB RAM under heavy load with realistic fluctuating values.
// Output → ~/Downloads/m5-ultra-gag.gif
//
// Standalone: no FFI, no Rust library — pure Swift + AppKit with local
// struct + Renderer copies so `swift render_gag.swift` Just Works.

import AppKit
import Foundation
import ImageIO
import UniformTypeIdentifiers

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
    pcpu_cores: 32,
    ecpu_cores: 8,
    gpu_cores: 80,
    total_ram: UInt64(1024) * (1 << 30)
)

let base = PmMetrics(
    sys_power: 268.4,
    cpu_power: 78.9,
    gpu_power: 142.1,
    ane_power: 6.3,
    dram_power: 14.7,
    cpu_temp: 94.0,
    gpu_temp: 88.0,
    pcpu_util: 0.98, pcpu_mhz: 5200,
    ecpu_util: 0.84, ecpu_mhz: 3100,
    gpu_util:  0.96, gpu_mhz:  2100,
    mem_used:  UInt64(964) * (1 << 30),
    mem_total: UInt64(1024) * (1 << 30),
    swap_used: UInt64(12)  * (1 << 30),
    swap_total: UInt64(16) * (1 << 30),
    fan_rpm: 4800, fan_max_rpm: 5800,
    interval_ms: 1002.0
)

let chip = "Apple M5 Ultra"
let host = "cosmos"

// MARK: - Organic fluctuation

func wobble(_ base: Float, amplitude: Float, frame: Int, phase: Double) -> Float {
    let t = Double(frame)
    let v = sin(t * 0.15 + phase) * 0.6
        + sin(t * 0.37 + phase * 1.7) * 0.25
        + sin(t * 0.73 + phase * 0.3) * 0.15
    return base + amplitude * Float(v)
}

func clamp01(_ v: Float) -> Float { min(max(v, 0), 1) }

func metricsForFrame(_ i: Int) -> PmMetrics {
    var m = base
    m.cpu_power  = max(0, wobble(base.cpu_power,  amplitude: 12, frame: i, phase: 0.0))
    m.gpu_power  = max(0, wobble(base.gpu_power,  amplitude: 18, frame: i, phase: 1.3))
    m.ane_power  = max(0, wobble(base.ane_power,  amplitude: 2.5, frame: i, phase: 2.7))
    m.dram_power = max(0, wobble(base.dram_power, amplitude: 3,  frame: i, phase: 3.1))
    m.sys_power  = m.cpu_power + m.gpu_power + m.ane_power + m.dram_power + wobble(26, amplitude: 4, frame: i, phase: 4.0)

    m.cpu_temp = wobble(base.cpu_temp, amplitude: 4, frame: i, phase: 0.5)
    m.gpu_temp = wobble(base.gpu_temp, amplitude: 3, frame: i, phase: 1.8)

    m.pcpu_util = clamp01(wobble(base.pcpu_util, amplitude: 0.06, frame: i, phase: 0.2))
    m.ecpu_util = clamp01(wobble(base.ecpu_util, amplitude: 0.10, frame: i, phase: 1.0))
    m.gpu_util  = clamp01(wobble(base.gpu_util,  amplitude: 0.08, frame: i, phase: 1.5))

    let pcpuOptions: [UInt32] = [5000, 5100, 5200, 5200, 5200, 5300]
    let ecpuOptions: [UInt32] = [2900, 3000, 3100, 3100, 3100, 3200]
    let gpuOptions:  [UInt32] = [2000, 2050, 2100, 2100, 2100, 2150]
    let fi = abs(i) % 60
    m.pcpu_mhz = pcpuOptions[fi % pcpuOptions.count]
    m.ecpu_mhz = ecpuOptions[(fi + 2) % ecpuOptions.count]
    m.gpu_mhz  = gpuOptions[(fi + 4) % gpuOptions.count]

    let memDelta = Int64(wobble(0, amplitude: 2, frame: i, phase: 5.0) * Float(1 << 30))
    m.mem_used = UInt64(clamping: Int64(base.mem_used) + memDelta)
    let swapDelta = Int64(wobble(0, amplitude: 0.5, frame: i, phase: 6.0) * Float(1 << 30))
    m.swap_used = UInt64(clamping: Int64(base.swap_used) + swapDelta)

    m.fan_rpm = UInt32(clamping: Int32(base.fan_rpm) + Int32(wobble(0, amplitude: 200, frame: i, phase: 3.5)))

    m.interval_ms = wobble(1002, amplitude: 8, frame: i, phase: 7.0)

    return m
}

// MARK: - Renderer (mirror of app/PowerMonitorMenuBar.swift)

enum Renderer {
    static let INNER = 60
    static let BAR_W = 24

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

// MARK: - Render one frame to CGImage

func renderFrame(_ i: Int) -> CGImage {
    let m = metricsForFrame(i)
    let ns = Renderer.frame(m: m, soc: soc, chip: chip, host: host)

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

    if let container = textView.textContainer, let lm = textView.layoutManager {
        lm.ensureLayout(for: container)
    }

    guard let rep = textView.bitmapImageRepForCachingDisplay(in: textView.bounds) else {
        fputs("error: could not build bitmap rep for frame \(i)\n", stderr)
        exit(1)
    }
    textView.cacheDisplay(in: textView.bounds, to: rep)

    guard let cgImage = rep.cgImage else {
        fputs("error: could not get CGImage for frame \(i)\n", stderr)
        exit(1)
    }
    return cgImage
}

// MARK: - Assemble animated GIF

let frameCount = 60
let frameDelay: Double = 0.15  // 150ms per frame → ~9s loop

let outPath = (NSHomeDirectory() as NSString).appendingPathComponent("Downloads/m5-ultra-gag.gif")
let outURL = URL(fileURLWithPath: outPath) as CFURL

guard let dest = CGImageDestinationCreateWithURL(outURL, "com.compuserve.gif" as CFString, frameCount, nil) else {
    fputs("error: could not create GIF destination\n", stderr)
    exit(1)
}

let gifProperties: [String: Any] = [
    kCGImagePropertyGIFDictionary as String: [
        kCGImagePropertyGIFLoopCount as String: 0  // loop forever
    ]
]
CGImageDestinationSetProperties(dest, gifProperties as CFDictionary)

let frameProperties: [String: Any] = [
    kCGImagePropertyGIFDictionary as String: [
        kCGImagePropertyGIFDelayTime as String: frameDelay
    ]
]

for i in 0..<frameCount {
    let img = renderFrame(i)
    CGImageDestinationAddImage(dest, img, frameProperties as CFDictionary)
    if (i + 1) % 10 == 0 {
        print("Rendered \(i + 1)/\(frameCount) frames")
    }
}

guard CGImageDestinationFinalize(dest) else {
    fputs("error: could not finalize GIF\n", stderr)
    exit(1)
}

let attrs = try? FileManager.default.attributesOfItem(atPath: outPath)
let size = (attrs?[.size] as? Int) ?? 0
print("Wrote: \(outPath)")
print("Frames: \(frameCount) @ \(Int(frameDelay * 1000))ms = \(String(format: "%.1f", Double(frameCount) * frameDelay))s loop")
print("Size: \(size / 1024) KB")
