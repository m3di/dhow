import AppKit
import DhowCore
import Observation

/// Hands the core's events, which arrive on its event thread, to the store
/// on the main queue, in order. Holds the store weakly; nothing here holds
/// the engine (ARCHITECTURE §9).
final class Listener: EventListener, @unchecked Sendable {
    /// Set before the engine opens; read only on the main queue.
    weak var store: DownloadStore?

    func onEvent(event: Event) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated { self.store?.apply(event) }
        }
    }
}

struct AppAlert: Identifiable {
    let id = UUID()
    var message: String
}

/// The Add dialog's request to open, with what to prefill.
struct AddPrefill: Identifiable {
    let id = UUID()
    var text: String
}

/// The engine and its lifecycle (ARCHITECTURE §9), and the commands the
/// views send. Engine calls run off the main thread: the synchronous ones
/// may ask the adapter for file tokens (M4.3), and the async ones run on
/// the engine's runtime anyway.
@MainActor @Observable
final class AppModel {
    let store = DownloadStore()
    /// Kept for the app's life: dropping it blocks the dropping thread, so
    /// it is never dropped, only shut down at quit.
    private(set) var engine: Engine?
    /// Why the engine couldn't open; the window says so instead of a list.
    private(set) var openFailure: String?
    var alert: AppAlert?
    var add: AddPrefill?

    @ObservationIgnored private let listener = Listener()
    @ObservationIgnored private var observers: [(NotificationCenter, NSObjectProtocol)] = []

    private struct Opened: Sendable {
        var engine: Engine
        var downloads: [DownloadSnapshot]
        var queues: [QueueSnapshot]
        var groups: [GroupSnapshot]
        var settings: DhowSettings
    }

    func start() {
        guard engine == nil, openFailure == nil else { return }
        listener.store = store
        let listener = listener
        Task {
            do {
                let opened = try await Task.detached { try Self.open(listener) }.value
                engine = opened.engine
                store.load(
                    downloads: opened.downloads, queues: opened.queues, groups: opened.groups,
                    settings: opened.settings)
                observeSystem()
            } catch {
                openFailure = ErrorText.message(error)
            }
        }
    }

    /// The CLI's directories (shared on purpose, ARCHITECTURE §2), or
    /// `DHOW_DATA_DIR` / `DHOW_LOG_DIR` as the CLI reads them.
    private nonisolated static func open(_ listener: Listener) throws -> Opened {
        let env = ProcessInfo.processInfo.environment
        let support = FileManager.default.urls(
            for: .applicationSupportDirectory, in: .userDomainMask)
        let data =
            env["DHOW_DATA_DIR"] ?? defaultDataDir()
            ?? support[0].appendingPathComponent("Dhow").path
        if let logs = env["DHOW_LOG_DIR"] ?? defaultLogDir() {
            // Once per process; a failure only loses the log.
            try? initLogging(dir: logs, filter: env["DHOW_LOG"])
        }
        let engine = try Engine.open(
            dataDir: data, config: EngineConfig(), listener: listener, adapter: nil)
        return Opened(
            engine: engine, downloads: try engine.list(), queues: try engine.listQueues(),
            groups: try engine.listGroups(), settings: try engine.settings())
    }

    /// Stops every run cleanly; the next launch continues them.
    func shutdown() async {
        removeObservers()
        await engine?.shutdown()
    }

    /// The engine's schedule sleeps on a monotonic clock, which stops while
    /// the Mac sleeps; it is told on wake, on clock and time-zone changes,
    /// and when the app becomes active (where the CLI's changes show up).
    private func observeSystem() {
        let workspace = NSWorkspace.shared.notificationCenter
        let center = NotificationCenter.default
        for (nc, name) in [
            (workspace, NSWorkspace.didWakeNotification),
            (center, Notification.Name.NSSystemClockDidChange),
            (center, Notification.Name.NSSystemTimeZoneDidChange),
            (center, NSApplication.didBecomeActiveNotification),
        ] {
            let token = nc.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.recheck() }
            }
            observers.append((nc, token))
        }
    }

    private func removeObservers() {
        for (center, token) in observers {
            center.removeObserver(token)
        }
        observers = []
    }

    func recheck() {
        guard let engine else { return }
        Task.detached { engine.recheck() }
    }

    // MARK: Commands

    /// Runs `op` for each download at once, off the main thread; the first
    /// failure is shown.
    private func each(
        _ ids: [String], _ op: @escaping @Sendable (Engine, String) async throws -> Void
    ) {
        guard let engine, !ids.isEmpty else { return }
        Task {
            let failure = await Task.detached { () -> Error? in
                await withTaskGroup(of: Error?.self) { group in
                    for id in ids {
                        group.addTask {
                            do {
                                try await op(engine, id)
                                return nil
                            } catch {
                                return error
                            }
                        }
                    }
                    var first: Error?
                    for await e in group where first == nil {
                        first = e
                    }
                    return first
                }
            }.value
            if let failure {
                show(failure)
            }
        }
    }

    func show(_ error: Error) {
        alert = AppAlert(message: ErrorText.message(error))
    }

    private func ids(_ ids: Set<String>, where ok: (DownloadState) -> Bool) -> [String] {
        ids.filter { store.downloads[$0].map { ok($0.state) } ?? false }
    }

    func pause(_ ids: Set<String>) {
        each(self.ids(ids, where: Actions.canPause)) { _ = try await $0.pause(id: $1) }
    }

    /// Paused and failed ones start now, by hand (ENGINE-SPEC §11), as do
    /// queued ones ("Start now").
    func resume(_ ids: Set<String>) {
        each(self.ids(ids, where: Actions.canResume)) { _ = try $0.resume(id: $1) }
    }

    /// A cancelled download can't continue, so its part file goes.
    func cancel(_ ids: Set<String>) {
        each(self.ids(ids, where: Actions.canCancel)) {
            _ = try await $0.cancel(id: $1, deletePartial: true)
        }
    }

    func remove(_ ids: Set<String>, deleteFiles: Bool) {
        each(Array(ids)) { try await $0.remove(id: $1, deleteFiles: deleteFiles) }
    }

    func move(_ ids: Set<String>, toQueue queue: String) {
        each(ids.filter { store.downloads[$0]?.queueId != queue }) {
            _ = try $0.moveToQueue(id: $1, queue: queue)
        }
    }

    func setSpeedLimit(_ ids: Set<String>, _ limit: UInt64?) {
        each(Array(ids)) { _ = try $0.setSpeedLimit(id: $1, limit: limit) }
    }

    /// Toolbar: everything unfinished stops, and everything paused starts.
    func pauseAll() {
        pause(Set(store.downloads.keys))
    }

    func resumeAll() {
        resume(Set(store.downloads.values.filter { $0.state == .paused }.map(\.id)))
    }

    func open(_ ids: Set<String>) {
        for id in ids {
            guard let d = store.downloads[id], d.state == .completed else { continue }
            NSWorkspace.shared.open(URL(fileURLWithPath: d.finalPath))
        }
    }

    /// The file, or the part file while unfinished, or failing both its
    /// folder.
    func showInFinder(_ ids: Set<String>) {
        let fm = FileManager.default
        let urls = ids.compactMap { store.downloads[$0] }.map { d in
            let path =
                [d.finalPath, d.partPath].first { fm.fileExists(atPath: $0) }
                ?? (d.finalPath as NSString).deletingLastPathComponent
            return URL(fileURLWithPath: path)
        }
        if !urls.isEmpty {
            NSWorkspace.shared.activateFileViewerSelecting(urls)
        }
    }

    func copyLinks(_ ids: Set<String>) {
        let links = store.order.filter(ids.contains).compactMap { store.downloads[$0]?.url }
        guard !links.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(links.joined(separator: "\n"), forType: .string)
    }

    // MARK: Add

    /// ⌘N and the toolbar: prefilled from the clipboard if it holds links
    /// (UI-GUIDELINES §4).
    func showAdd() {
        guard engine != nil else { return }
        let clip = NSPasteboard.general.string(forType: .string) ?? ""
        add = AddPrefill(text: AddModel.links(in: clip).joined(separator: "\n"))
    }

    /// Paste in the window: opens Add when the clipboard holds links.
    func paste() {
        let clip = NSPasteboard.general.string(forType: .string) ?? ""
        let links = AddModel.links(in: clip)
        guard engine != nil, !links.isEmpty else { return }
        add = AddPrefill(text: links.joined(separator: "\n"))
    }
}

/// Which actions apply to a download in a state, for menus and commands.
enum Actions {
    static func canPause(_ s: DownloadState) -> Bool {
        s == .active || s == .waiting || s == .queued
    }

    static func canResume(_ s: DownloadState) -> Bool {
        s == .paused || s == .failed || s == .queued
    }

    static func canCancel(_ s: DownloadState) -> Bool {
        !s.isFinished && s != .completing
    }
}
