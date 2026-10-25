import AppKit
import DhowCore
import IOKit.pwr_mgt
import SwiftUI

/// What the Mac can do when a queue runs dry (ENGINE-SPEC §11
/// `on_complete`: the shell decides what's possible). Hibernate is sleep:
/// whether sleep hibernates is the system's `hibernatemode`.
@MainActor
protocol SystemActions: Sendable {
    /// `nil` when done; else why not, for the log.
    func sleep() -> String?
    func shutDown() -> String?
    /// Whether Dhow may send the shut-down request (Automation, TCC);
    /// `ask` lets macOS ask the user now. Blocks; call off the main thread.
    nonisolated func mayShutDown(ask: Bool) -> Bool
}

struct MacSystemActions: SystemActions {
    func sleep() -> String? {
        let port = IOPMFindPowerManagement(mach_port_t(MACH_PORT_NULL))
        guard port != 0 else { return "no power management connection" }
        defer { IOServiceClose(port) }
        let result = IOPMSleepSystem(port)
        return result == kIOReturnSuccess ? nil : "IOPMSleepSystem returned \(result)"
    }

    /// The same request as  > Shut Down, without its dialog: apps are asked
    /// to quit (Dhow stops its runs cleanly first). An Apple event to
    /// loginwindow, which macOS may gate behind Automation consent; the
    /// queue editor asks for it when Shut Down is chosen, not at night.
    func shutDown() -> String? {
        var error: NSDictionary?
        let script = NSAppleScript(source: "tell application \"loginwindow\" to «event aevtshut»")
        guard script?.executeAndReturnError(&error) != nil else {
            return error.map { String(describing: $0) } ?? "the script didn't run"
        }
        return nil
    }

    nonisolated func mayShutDown(ask: Bool) -> Bool {
        let target = NSAppleEventDescriptor(bundleIdentifier: "com.apple.loginwindow")
        guard let desc = target.aeDesc else { return false }
        // kCoreEventClass 'aevt', the 'shut' event.
        let status = AEDeterminePermissionToAutomateTarget(
            desc, AEEventClass(0x6165_7674), AEEventID(0x7368_7574), ask)
        return status == noErr
    }
}

/// The countdown before `on_complete` acts (never silently): visible,
/// cancellable, and called off by any download starting meanwhile.
struct Countdown: Equatable {
    enum Action: Equatable { case sleep, shutDown }

    var action: Action
    var queue: String
    var secondsLeft: Int

    static func action(for onComplete: OnComplete) -> Action? {
        switch onComplete {
        case .nothing: nil
        case .sleep, .hibernate: .sleep
        case .shutdown: .shutDown
        }
    }
}

/// The countdown's panel: in front of everything, whatever window is open.
@MainActor
final class CountdownPanel {
    private var panel: NSPanel?

    func show(_ model: AppModel) {
        if panel == nil {
            let panel = NSPanel(
                contentRect: NSRect(x: 0, y: 0, width: 380, height: 150),
                styleMask: [.titled, .nonactivatingPanel], backing: .buffered, defer: false)
            panel.level = .floating
            // On whatever Space, and over a full-screen app.
            panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            panel.isReleasedWhenClosed = false
            panel.contentView = NSHostingView(rootView: CountdownView().environment(model))
            panel.center()
            self.panel = panel
        }
        panel?.title = tr("app.name")
        panel?.orderFrontRegardless()
        NSApp.requestUserAttention(.criticalRequest)
    }

    func hide() {
        panel?.close()
        panel = nil
    }
}

private struct CountdownView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            if let c = model.countdown {
                Text(Self.title(c))
                    .font(.headline)
                    .monospacedDigit()
                Text(tr("countdown.because \(c.queue)"))
                    .foregroundStyle(.secondary)
                HStack {
                    Spacer()
                    Button(tr("button.cancel"), role: .cancel) { model.cancelCountdown() }
                        .keyboardShortcut(.cancelAction)
                    Button(
                        c.action == .sleep ? tr("countdown.sleepNow") : tr("countdown.shutDownNow")
                    ) {
                        model.finishCountdown()
                    }
                }
            }
        }
        .padding(20)
        .frame(width: 380)
    }

    static func title(_ c: Countdown) -> String {
        switch c.action {
        case .sleep: tr("countdown.sleep \(c.secondsLeft)")
        case .shutDown: tr("countdown.shutDown \(c.secondsLeft)")
        }
    }
}
