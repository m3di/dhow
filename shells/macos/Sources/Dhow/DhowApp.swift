import AppKit
import SwiftUI

@main
struct DhowApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate

    var body: some Scene {
        // One main window (UI-GUIDELINES §2), not a window group.
        Window(tr("app.name"), id: "main") {
            MainWindow()
                .environment(delegate.model)
        }
        .defaultSize(width: 1000, height: 560)
        .commands { DhowCommands(model: delegate.model) }
        Settings {
            SettingsView()
                .environment(delegate.model)
        }
    }
}

/// The app's lifecycle around the engine's (ARCHITECTURE §9), and its
/// place in macOS: notifications, the Dock tile, the Services menu.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()
    private var terminateSignal: DispatchSourceSignal?
    private var dock: DockTile?
    private let services = Services()

    func applicationDidFinishLaunching(_ notification: Notification) {
        if let notifier = UserNotifier.make() {
            notifier.onOpen = { [model] id, reveal in model.openFromNote(id, reveal: reveal) }
            notifier.install()
            model.notifier = notifier
        }
        model.countdownPanel = CountdownPanel()
        model.start()
        let dock = DockTile(store: model.store)
        dock.start()
        self.dock = dock
        services.model = model
        NSApp.servicesProvider = services
        // `kill` (and launchd) quit as ⌘Q does: through the shutdown below.
        // Not from inside this main-queue block: the shutdown's reply needs
        // the main queue while `terminate` waits for it.
        signal(SIGTERM, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        source.setEventHandler {
            NSApp.perform(
                #selector(NSApplication.terminate(_:)), with: nil, afterDelay: 0,
                inModes: [.common])
        }
        source.resume()
        terminateSignal = source
    }

    /// Closing the window hides; the Dock icon stays (§2).
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// A click on the Dock icon with the window closed brings it back.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows {
            model.showMainWindow()
        }
        return true
    }

    /// Runs stop cleanly before the process ends (committed, back to
    /// `Queued`, so the next launch continues them). An engine still
    /// opening is waited for, then shut down.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        model.cancelCountdown()
        guard model.engine != nil || model.opening != nil else { return .terminateNow }
        Task {
            await model.shutdown()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}

/// "Download with Dhow" in the Services menu (UI-GUIDELINES §8 macOS):
/// the selected links open the Add dialog.
@MainActor
final class Services: NSObject {
    weak var model: AppModel?

    @objc func downloadURLs(
        _ pboard: NSPasteboard, userData: String?,
        error: AutoreleasingUnsafeMutablePointer<NSString?>
    ) {
        let urls = (pboard.readObjects(forClasses: [NSURL.self]) as? [URL] ?? []).map(
            \.absoluteString)
        let text = pboard.string(forType: .string) ?? ""
        model?.add(links: AddModel.links(in: (urls + [text]).joined(separator: "\n")))
    }
}
