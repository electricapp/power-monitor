import SwiftUI
import Foundation

// Pure layout / box drawing for the menu-bar dashboard — a glyph-for-glyph
// port of `src/main.rs`. Kept FFI-free so the box-width invariant can be
// checked standalone at build time (see `layout_check.swift`, gated in
// `build.sh`). The FFI-dependent `frame()` / `version` live in
// `PowerMonitorMenuBar.swift` as `extension Renderer`.
enum Renderer {
    static let INNER = 56
    static let BAR_W = 24

    // ANSI-equivalent colors, tuned for a dark popover background.
    static let green  = Color(red: 0.30, green: 0.78, blue: 0.34)
    static let yellow = Color(red: 0.92, green: 0.77, blue: 0.24)
    static let red    = Color(red: 0.93, green: 0.35, blue: 0.35)
    static let cyan   = Color(red: 0.38, green: 0.82, blue: 0.82)
    static let dim    = Color(white: 0.48)
    static let fg     = Color(white: 0.92)

    // MARK: helpers

    static func heat(_ frac: Float) -> Color {
        frac < 0.40 ? green : frac < 0.75 ? yellow : red
    }

    static func tempColor(_ t: Float) -> Color {
        t < 70 ? green : t < 85 ? yellow : red
    }

    static func tint(_ s: String, _ c: Color, bold: Bool = false) -> AttributedString {
        var a = AttributedString(s)
        a.foregroundColor = c
        if bold {
            a.font = .system(size: 11, weight: .bold, design: .monospaced)
        }
        return a
    }

    static func bar(_ val: Float, max maxV: Float) -> AttributedString {
        let frac = min(Swift.max(maxV == 0 ? 0 : val / maxV, 0), 1)
        let n = Int((frac * Float(BAR_W)).rounded())
        var out = tint(String(repeating: "█", count: n), heat(frac))
        out.append(tint(String(repeating: "░", count: BAR_W - n), dim))
        return out
    }

    static func leftCol(_ label: String, pct: Float) -> AttributedString {
        let p = min(Swift.max(pct, 0), 100)
        var out = AttributedString("  ")
        out.append(tint(label.padding(toLength: 5, withPad: " ", startingAt: 0), fg, bold: true))
        out.append(AttributedString(" "))
        out.append(tint(String(format: "%3.0f%%", p), dim))
        out.append(AttributedString("  "))
        return out
    }

    static func powerRow(_ label: String, _ w: Float, max maxW: Float) -> AttributedString {
        var out = leftCol(label, pct: w / maxW * 100)
        out.append(bar(w, max: maxW))
        // Single space before "W" (mirrors src/main.rs): keeps the temp rows
        // within INNER for a 3-digit (≥100 °C) reading instead of overflowing
        // the right border by one column.
        out.append(tint(String(format: "  %5.2f W", w), cyan))
        return out
    }

    static func powerRowTemp(_ label: String, _ w: Float, max maxW: Float, temp: Float) -> AttributedString {
        var out = powerRow(label, w, max: maxW)
        out.append(tint("  (", dim))
        out.append(tint(String(format: "%.0f°C", temp), tempColor(temp)))
        out.append(tint(")", dim))
        return out
    }

    static func utilRow(_ label: String, _ util: Float, _ mhz: UInt32) -> AttributedString {
        var out = leftCol(label, pct: util * 100)
        out.append(bar(util, max: 1))
        out.append(tint(String(format: "  %4u MHz", mhz), cyan))
        return out
    }

    static func fanRow(_ rpm: UInt32, maxRpm: UInt32) -> AttributedString {
        if maxRpm == 0 {
            // Greyed-out placeholder to keep tile height consistent across hardware.
            var out = AttributedString("  ")
            out.append(tint("FAN", fg, bold: true))
            out.append(tint("    —  \(String(repeating: "─", count: BAR_W))  fanless", dim))
            return out
        }
        let duty = min(Swift.max(Float(rpm) / Float(maxRpm), 0), 1)
        var out = leftCol("FAN", pct: duty * 100)
        out.append(bar(duty, max: 1))
        out.append(tint(String(format: "  %4u RPM", rpm), cyan))
        return out
    }

    static func memRow(_ label: String, used: UInt64, total: UInt64) -> AttributedString {
        let pct = total > 0 ? Float(used) / Float(total) : 0
        var out = leftCol(label, pct: pct * 100)
        out.append(bar(pct, max: 1))
        out.append(tint(String(format: "  %4.1f", Double(used) / Double(1 << 30)), cyan))
        out.append(tint(String(format: " / %4.1f GB", Double(total) / Double(1 << 30)), dim))
        return out
    }

    // MARK: box drawing

    static func visualWidth(_ s: AttributedString) -> Int {
        String(s.characters).count
    }

    static func boxRow(_ content: AttributedString) -> AttributedString {
        let pad = Swift.max(INNER - visualWidth(content), 0)
        var out = tint("│", dim)
        out.append(content)
        out.append(AttributedString(String(repeating: " ", count: pad)))
        out.append(tint("│\n", dim))
        return out
    }

    static func boxEmpty() -> AttributedString {
        tint("│\(String(repeating: " ", count: INNER))│\n", dim)
    }

    static func boxRule() -> AttributedString {
        tint("│\(String(repeating: "─", count: INNER))│\n", dim)
    }

    static func timeStamp() -> String {
        let m = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"]
        let c = Calendar.current.dateComponents([.hour, .minute, .day, .month], from: Date())
        let mon = m[(c.month ?? 1) - 1]
        return String(format: "%02d:%02d %02d-\(mon)", c.hour ?? 0, c.minute ?? 0, c.day ?? 0)
    }
}
