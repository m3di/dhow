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
    }
}

/// The app's lifecycle around the engine's (ARCHITECTURE §9).
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = AppModel()
    private var terminateSignal: DispatchSourceSignal?

    func applicationDidFinishLaunching(_ notification: Notification) {
        model.start()
        // `kill` (and launchd) quit as ⌘Q does: through the shutdown below.
        // Not from inside this main-queue block: the shutdown's reply needs
        // the main queue while `terminate` waits for it.
        signal(SIGTERM, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        source.setEventHandler {
            NSApp.perform(#selector(NSApplication.terminate(_:)), with: nil, afterDelay: 0)
        }
        source.resume()
        terminateSignal = source
    }

    /// Closing the window hides; the Dock icon stays (§2).
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Runs stop cleanly before the process ends (committed, back to
    /// `Queued`, so the next launch continues them).
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard model.engine != nil else { return .terminateNow }
        Task {
            await model.shutdown()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}
