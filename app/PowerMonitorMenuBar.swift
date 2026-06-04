// Menu bar app mirroring `power-monitor`'s terminal dashboard.
// FFI into libpower_monitor via the bridging header; monospaced
// AttributedString reproduces the TUI layout glyph-for-glyph.
//
// Left-click  → popover with the dashboard.
// Right-click → NSMenu with Quit. (SwiftUI MenuBarExtra doesn't give us
// right-click routing, so the status item is driven by AppKit directly.)

import SwiftUI
import AppKit
import Foundation

// MARK: - App entry

@main
struct PowerMonitorApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    var body: some Scene {
        // Scene placeholder — LSUIElement hides any window chrome, and the
        // real UI lives in AppDelegate's NSStatusItem + NSPopover.
        Settings { EmptyView() }
    }
}

// MARK: - Status item driver

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem!
    private var popover: NSPopover!
    private var model: Model!
    // Fires on any mouse-down anywhere else on screen while the popover is up.
    private var outsideClickMonitor: Any?

    func applicationDidFinishLaunching(_ notification: Notification) {
        model = Model()

        popover = NSPopover()
        popover.behavior = .transient
        let hosting = NSHostingController(
            rootView: PopoverView().environmentObject(model)
        )
        hosting.sizingOptions = [.preferredContentSize]
        popover.contentViewController = hosting

        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = statusItem.button {
            button.image = NSImage(
                systemSymbolName: "bolt.circle",
                accessibilityDescription: "Power Monitor"
            )
            button.action = #selector(statusItemClicked(_:))
            button.target = self
            // Receive both click types so we can route them ourselves.
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }
    }

    @objc private func statusItemClicked(_ sender: NSStatusBarButton) {
        let type = NSApp.currentEvent?.type
        if type == .rightMouseUp {
            showContextMenu(sender)
        } else {
            togglePopover(sender)
        }
    }

    private func showContextMenu(_ sender: NSStatusBarButton) {
        // Close any open popover so menu and popover don't overlap.
        if popover.isShown { popover.performClose(nil) }

        let menu = NSMenu()
        let quit = NSMenuItem(
            title: "Quit PowerMonitor",
            action: #selector(quit),
            keyEquivalent: "q"
        )
        quit.target = self
        menu.addItem(quit)
        menu.popUp(
            positioning: nil,
            at: NSPoint(x: 0, y: sender.bounds.height + 4),
            in: sender
        )
    }

    private func togglePopover(_ sender: NSStatusBarButton) {
        if popover.isShown {
            closePopover()
        } else {
            openPopover(sender)
        }
    }

    private func openPopover(_ sender: NSStatusBarButton) {
        popover.show(relativeTo: sender.bounds, of: sender, preferredEdge: .minY)
        popover.contentViewController?.view.window?.becomeKey()

        // NSPopover.transient closes only on outside clicks inside this app;
        // an LSUIElement app has no other windows, so a global event monitor
        // is what actually closes it when the user clicks elsewhere on screen.
        outsideClickMonitor = NSEvent.addGlobalMonitorForEvents(
            matching: [.leftMouseDown, .rightMouseDown]
        ) { [weak self] _ in
            Task { @MainActor in self?.closePopover() }
        }
    }

    private func closePopover() {
        popover.performClose(nil)
        if let token = outsideClickMonitor {
            NSEvent.removeMonitor(token)
            outsideClickMonitor = nil
        }
    }

    @objc private func quit() {
        NSApp.terminate(nil)
    }
}

// MARK: - Popover view

struct PopoverView: View {
    @EnvironmentObject private var model: Model

    var body: some View {
        Text(model.rendered)
            .font(.system(size: 11, weight: .regular, design: .monospaced))
            .foregroundStyle(Color(white: 0.92))
            .padding(12)
            .background(Color(white: 0.08))
            .fixedSize()
    }
}

// MARK: - Sampler model

@MainActor
final class Model: ObservableObject {
    @Published var rendered: AttributedString = AttributedString("  opening sampler…")

    init() {
        // Sampler state lives in a detached producer task; rendered snapshots
        // flow out via AsyncStream. Nothing about the sampler crosses actor
        // boundaries except `AttributedString` (which is Sendable).
        let stream = Self.renderStream()
        Task.detached(priority: .userInitiated) { [weak self] in
            for await body in stream {
                await MainActor.run { [weak self] in
                    self?.rendered = body
                }
            }
        }
    }

    // Process-lifetime: the producer owns the handle. Freeing on deinit would
    // race with an in-flight sample; process exit reclaims it anyway.

    nonisolated private static func renderStream() -> AsyncStream<AttributedString> {
        AsyncStream { continuation in
            Task.detached(priority: .userInitiated) {
                guard let h = pm_sampler_new() else {
                    continuation.yield(AttributedString(
                        "  failed to open sampler (SMC/IOReport unavailable)"
                    ))
                    continuation.finish()
                    return
                }

                var socInfo = PmSocInfo(
                    pcpu_cores: 0, ecpu_cores: 0, gpu_cores: 0, total_ram: 0
                )
                _ = pm_sampler_soc_info(h, &socInfo)

                var chipBuf = [UInt8](repeating: 0, count: 64)
                let n = chipBuf.withUnsafeMutableBufferPointer { bp in
                    pm_sampler_chip_name(h, bp.baseAddress, bp.count)
                }
                let chip = (n > 0 ? String(bytes: chipBuf.prefix(n), encoding: .utf8) : nil)
                    ?? "Apple Silicon"

                let raw = ProcessInfo.processInfo.hostName
                let host = raw.hasSuffix(".local") ? String(raw.dropLast(6)) : raw

                while !Task.isCancelled {
                    var m = PmMetrics(
                        sys_power: 0, cpu_power: 0, gpu_power: 0, ane_power: 0, dram_power: 0,
                        cpu_temp: 0, gpu_temp: 0,
                        pcpu_util: 0, pcpu_mhz: 0, ecpu_util: 0, ecpu_mhz: 0,
                        gpu_util: 0, gpu_mhz: 0,
                        mem_used: 0, mem_total: 0, swap_used: 0, swap_total: 0,
                        fan_rpm: 0, fan_max_rpm: 0,
                        interval_ms: 0
                    )
                    guard pm_sampler_sample(h, 1000, &m) else {
                        continuation.finish()
                        return
                    }
                    let body = Renderer.frame(m: m, soc: socInfo, chip: chip, host: host)
                    continuation.yield(body)
                }
                continuation.finish()
            }
        }
    }
}

// MARK: - Renderer FFI bits — pure layout / box drawing lives in BoxLayout.swift.

extension Renderer {

    // MARK: frame

    // Single source of truth — `pm_version()` returns CARGO_PKG_VERSION.
    static let version: String = {
        guard let cstr = pm_version() else { return "?" }
        return String(cString: cstr)
    }()

    static func frame(m: PmMetrics, soc: PmSocInfo, chip: String, host: String) -> AttributedString {
        let totalGb = Int((Double(soc.total_ram) / Double(1 << 30)).rounded())
        let title: String
        if soc.gpu_cores > 0 {
            title = " v\(version) · \(chip) · \(soc.pcpu_cores)P + \(soc.ecpu_cores)E · \(soc.gpu_cores) GPU · \(totalGb)GB "
        } else {
            title = " v\(version) · \(chip) · \(soc.pcpu_cores)P + \(soc.ecpu_cores)E · \(totalGb)GB "
        }

        var out = AttributedString()
        let topPad = Swift.max(INNER + 2 - title.count - 3, 0)
        out.append(tint("╭─", dim))
        out.append(tint(title, fg, bold: true))
        out.append(tint(String(repeating: "─", count: topPad) + "╮\n", dim))

        out.append(boxEmpty())
        out.append(boxRow(powerRow("SYS", m.sys_power, max: 40)))
        out.append(boxRow(fanRow(m.fan_rpm, maxRpm: m.fan_max_rpm)))
        out.append(boxRule())
        out.append(boxRow(powerRowTemp("GPU", m.gpu_power, max: 16, temp: m.gpu_temp)))
        out.append(boxRow(powerRowTemp("CPU", m.cpu_power, max: 20, temp: m.cpu_temp)))
        out.append(boxRow(powerRow("ANE", m.ane_power, max: 8)))
        out.append(boxRow(powerRow("DRAM", m.dram_power, max: 5)))

        out.append(boxEmpty())
        out.append(boxRow(utilRow("PCPU", m.pcpu_util, m.pcpu_mhz)))
        out.append(boxRow(utilRow("ECPU", m.ecpu_util, m.ecpu_mhz)))
        out.append(boxRow(utilRow("GPU", m.gpu_util, m.gpu_mhz)))

        out.append(boxEmpty())
        out.append(boxRow(memRow("MEM", used: m.mem_used, total: m.mem_total)))
        out.append(boxRow(memRow("SWAP", used: m.swap_used, total: m.swap_total)))

        out.append(boxEmpty())

        let intervalStr = String(format: " %.0f ms ", m.interval_ms)
        let sysStr = " \"\(host)\" \(timeStamp()) "
        let bpad = Swift.max(INNER + 2 - sysStr.count - intervalStr.count - 2, 0)
        out.append(tint("╰\(sysStr)\(String(repeating: "─", count: bpad))\(intervalStr)╯", dim))

        return out
    }
}
