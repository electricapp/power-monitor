import Foundation

// Build-time guard mirroring `rows_never_exceed_box_width` in src/main.rs:
// every dashboard row's visible content must fit within INNER, or it overflows
// the box's right border. The temperature rows are the widest; a 3-digit
// (≥100 °C) reading is the case that used to break the box. Compiled against
// BoxLayout.swift (FFI-free) and run as a gate in build.sh.
@main
struct LayoutCheck {
    static func rowWidths() -> [(String, Int)] {
        var rows: [(String, Int)] = []
        for t in [Float(46), 100, 105, .nan] {
            rows.append((
                "GPU @ \(t)°C",
                Renderer.visualWidth(Renderer.powerRowTemp("GPU", 12.34, max: 16, temp: t))
            ))
        }
        rows.append(("SYS", Renderer.visualWidth(Renderer.powerRow("SYS", 199.99, max: 40))))
        rows.append(("PCPU", Renderer.visualWidth(Renderer.utilRow("PCPU", 1.0, 9999))))
        rows.append(("MEM", Renderer.visualWidth(
            Renderer.memRow("MEM", used: UInt64(99) << 30, total: UInt64(128) << 30))))
        rows.append(("FAN", Renderer.visualWidth(Renderer.fanRow(6550, maxRpm: 6550))))
        return rows
    }

    static func main() {
        let overflowing = rowWidths().filter { $0.1 > Renderer.INNER }
        for (label, width) in overflowing {
            FileHandle.standardError.write(
                Data("layout: \(label) width \(width) > INNER \(Renderer.INNER)\n".utf8))
        }
        if !overflowing.isEmpty {
            FileHandle.standardError.write(
                Data("layout check FAILED (\(overflowing.count) row(s) overflow the box)\n".utf8))
            exit(1)
        }
        print("layout check OK — all rows ≤ INNER (\(Renderer.INNER))")
    }
}
