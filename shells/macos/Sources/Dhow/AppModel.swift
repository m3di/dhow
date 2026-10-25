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

/// What Pause All stopped, so Resume All puts it back (UI-GUIDELINES §3):
/// the queues it stopped with the run state each had, and the downloads
/// that were running.
struct PauseAllRecord: Equatable {
    var queues: [String: RunState] = [:]
    var runs: Set<String> = []
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
    /// Set by Pause All until Resume All.
    private(set) var pausedAll: PauseAllRecord?

    @ObservationIgnored let listener = Listener()
    @ObservationIgnored private var observers: [(NotificationCenter, NSObjectProtocol)] = []
    /// While the engine opens: quitting waits for it.
    @ObservationIgnored private(set) var opening: Task<Void, Never>?

    init() {
        listener.store = store
        store.unknownGroup = { [weak self] in self?.refreshGroups() }
    }

    func start() {
        guard engine == nil, opening == nil, openFailure == nil else { return }
        let listener = listener
        opening = Task {
            do {
                let engine = try await Task.detached { try Self.open(listener) }.value
                try await attach(engine)
                observeSystem()
            } catch {
                openFailure = ErrorText.message(error)
            }
            opening = nil
        }
    }

    /// The CLI's directories (shared on purpose, ARCHITECTURE §2), or
    /// `DHOW_DATA_DIR` / `DHOW_LOG_DIR` as the CLI reads them.
    private nonisolated static func open(_ listener: Listener) throws -> Engine {
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
        return try Engine.open(
            dataDir: data, config: EngineConfig(), listener: listener, adapter: nil)
    }

    private struct Loaded: Sendable {
        var downloads: [DownloadSnapshot]
        var queues: [QueueSnapshot]
        var groups: [GroupSnapshot]
        var settings: DhowSettings
    }

    /// Loads the store from an engine whose listener is `listener`.
    func attach(_ engine: Engine) async throws {
        let loaded = try await Task.detached {
            Loaded(
                downloads: try engine.list(), queues: try engine.listQueues(),
                groups: try engine.listGroups(), settings: try engine.settings())
        }.value
        self.engine = engine
        store.load(
            downloads: loaded.downloads, queues: loaded.queues, groups: loaded.groups,
            settings: loaded.settings)
    }

    /// Stops every run cleanly; the next launch continues them. A quit
    /// while the engine opens waits for it.
    func shutdown() async {
        await opening?.value
        removeObservers()
        await engine?.shutdown()
    }

    /// The engine's schedule sleeps on a monotonic clock, which stops while
    /// the Mac sleeps; it is told on wake, on clock and time-zone changes,
    /// and when the app becomes active, which also picks up what the CLI
    /// changed meanwhile.
    private func observeSystem() {
        let workspace = NSWorkspace.shared.notificationCenter
        let center = NotificationCenter.default
        for (nc, name) in [
            (workspace, NSWorkspace.didWakeNotification),
            (center, Notification.Name.NSSystemClockDidChange),
            (center, Notification.Name.NSSystemTimeZoneDidChange),
        ] {
            let token = nc.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.recheck() }
            }
            observers.append((nc, token))
        }
        let token = center.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        observers.append((center, token))
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

    /// `recheck`, then the list read again: the CLI's changes to downloads
    /// the app already knows (a `dhow pause`, a finished `dhow resume`)
    /// come as no event.
    func refresh() {
        guard let engine else { return }
        store.beginReconcile()
        Task {
            let list = await Task.detached { () -> [DownloadSnapshot]? in
                engine.recheck()
                return try? engine.list()
            }.value
            store.reconcile(list)
        }
    }

    private func refreshGroups() {
        guard let engine else { return }
        Task {
            if let groups = await Task.detached(operation: { try? engine.listGroups() }).value {
                store.setGroups(groups)
            }
        }
    }

    // MARK: Commands

    /// Runs `op` for each download at once, off the main thread; the first
    /// failure is shown. Of several, one the CLI runs is skipped quietly.
    private func each(
        _ ids: [String], _ op: @escaping @Sendable (Engine, String) async throws -> Void
    ) {
        guard let engine, !ids.isEmpty else { return }
        let bulk = ids.count > 1
        Task {
            let failure = await Task.detached { () -> Error? in
                await withTaskGroup(of: Error?.self) { group in
                    for id in ids {
                        group.addTask {
                            do {
                                try await op(engine, id)
                                return nil
                            } catch DhowError.RunningElsewhere where bulk {
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

    /// A cancelled download can't continue, so its part file goes (the
    /// menu asks first).
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

    // MARK: Pause All, Resume All

    /// Pause All can do something: a queue still runs or may start work.
    var canPauseAll: Bool {
        store.queues.contains { $0.runState != .stopped }
            && store.downloads.values.contains { $0.state.isRunning || $0.state == .queued }
    }

    var canResumeAll: Bool {
        pausedAll != nil || store.queues.contains { $0.runState == .stopped }
    }

    /// Stops every queue (`stop_queue`, which also stops the downloads
    /// started by hand, back to `Queued`), remembering how each ran and
    /// what ran. Downloads the user paused stay paused; nothing is marked
    /// paused, so the queues keep their order and concurrency.
    func pauseAll() async {
        guard engine != nil else { return }
        var record = pausedAll ?? PauseAllRecord()
        record.runs.formUnion(store.downloads.values.filter { $0.state.isRunning }.map(\.id))
        let queues = store.queues.filter { $0.runState != .stopped }
        for q in queues where record.queues[q.id] == nil {
            record.queues[q.id] = q.runState
        }
        pausedAll = record
        await bulk(queues.map(\.id)) { _ = try $0.stopQueue(id: $1) }
    }

    /// Undoes Pause All: each queue back to how it ran (its schedule, or
    /// started), then what ran started again (ENGINE-SPEC §11: by hand, as
    /// they had all run at once before). Without a Pause All to undo (it was
    /// before a relaunch), stopped queues go back to their schedules.
    func resumeAll() async {
        let record =
            pausedAll
            ?? PauseAllRecord(
                queues: Dictionary(
                    store.queues.filter { $0.runState == .stopped }.map { ($0.id, RunState.auto) },
                    uniquingKeysWith: { a, _ in a }))
        pausedAll = nil
        await bulk(Array(record.queues)) { engine, entry in
            switch entry.value {
            case .started: _ = try engine.startQueue(id: entry.key)
            case .auto, .stopped: _ = try engine.resumeSchedule(id: entry.key)
            }
        }
        let runs = record.runs.filter { store.downloads[$0]?.state == .queued }
        await bulk(Array(runs)) { _ = try $0.resume(id: $1) }
    }

    /// Like `each`, awaited; a vanished queue or download is skipped.
    private func bulk<T: Sendable>(
        _ items: [T], _ op: @escaping @Sendable (Engine, T) throws -> Void
    ) async {
        guard let engine, !items.isEmpty else { return }
        let failure = await Task.detached { () -> Error? in
            var first: Error?
            for item in items {
                do {
                    try op(engine, item)
                } catch DhowError.RunningElsewhere, DhowError.NotFound {
                } catch {
                    first = first ?? error
                }
            }
            return first
        }.value
        if let failure {
            show(failure)
        }
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
