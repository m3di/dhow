import AppKit
import DhowCore
import Foundation
import Testing

@testable import Dhow

@MainActor
final class RecordingNotifier: Notifier {
    var notes: [Note] = []
    func post(_ note: Note) { notes.append(note) }
}

@MainActor
final class RecordingSystem: SystemActions {
    var done: [String] = []
    var fails = false
    func sleep() -> String? {
        done.append("sleep")
        return fails ? "no" : nil
    }
    func shutDown() -> String? {
        done.append("shutDown")
        return fails ? "no" : nil
    }
    nonisolated func mayShutDown(ask: Bool) -> Bool { true }
}

/// Notifications, `on_complete` and the Dock (UI-GUIDELINES §2, §8 macOS),
/// driven by store events; nothing here reaches UserNotifications or the
/// power manager.
@MainActor
@Suite struct IntegrationTests {
    let core: TempEngine
    let queues: [QueueSnapshot]
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        queues = try core.engine.listQueues()
        settings = try core.engine.settings()
    }

    func queue(
        _ q: QueueSnapshot, queued: UInt32, running: UInt32, onComplete: OnComplete = .nothing
    )
        -> QueueSnapshot
    {
        var q = q
        q.queued = queued
        q.running = running
        q.onComplete = onComplete
        return q
    }

    /// A model whose store has `downloads` and the default queue, with a
    /// recording notifier and system.
    func model(_ downloads: [DownloadSnapshot], onComplete: OnComplete = .nothing) -> (
        AppModel, RecordingNotifier, RecordingSystem
    ) {
        let model = AppModel()
        let notifier = RecordingNotifier()
        let system = RecordingSystem()
        model.notifier = notifier
        model.system = system
        model.countdownTick = .milliseconds(5)
        model.countdownLength = 3
        let busy = queue(
            queues[0], queued: 0, running: UInt32(downloads.count), onComplete: onComplete)
        let inQueue = downloads.map {
            var d = $0
            d.queueId = busy.id
            return d
        }
        model.store.load(downloads: inQueue, queues: [busy], groups: [], settings: settings)
        return (model, notifier, system)
    }

    func until(_ what: String, _ ok: () -> Bool) async throws {
        for _ in 0..<300 where !ok() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(ok(), "\(what)")
    }

    @Test func aCompletedOrFailedDownloadNotifiesOnce() async {
        let (model, notifier, _) = model([download("a", .active), download("b", .active)])
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        model.store.apply(.stateChanged(id: "a", from: .completed, to: .completed, reason: nil))
        model.store.apply(.stateChanged(id: "b", from: .active, to: .failed, reason: .noSpace))
        #expect(notifier.notes.map(\.kind) == [.completed, .failed])
        #expect(notifier.notes[0].body == "a")
        #expect(notifier.notes[0].downloadId == "a")
        #expect(notifier.notes[1].body.contains(RowStatus.failure(.noSpace)))
        // Pausing, queueing: routine, quiet.
        model.store.apply(.stateChanged(id: "b", from: .failed, to: .queued, reason: nil))
        #expect(notifier.notes.count == 2)
        await core.close()
    }

    @Test func aGroupNotifiesOnceWhenAllOfItIsDone() async {
        let (model, notifier, _) = model([
            download("a", .active, group: "g"), download("b", .active, group: "g"),
            download("c", .cancelled, group: "g"),
        ])
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        #expect(notifier.notes.isEmpty)
        model.store.apply(.stateChanged(id: "b", from: .active, to: .completed, reason: nil))
        #expect(notifier.notes.map(\.kind) == [.group])
        await core.close()
    }

    @Test func notificationsTheUserTurnedOffStayOff() async {
        let (model, _, _) = model([download("a", .active)])
        let change = StoreChange.finished(download("a", .completed))
        var off = NotePolicy.Settings()
        off.completed = false
        #expect(NotePolicy.notes(for: change, in: model.store, off).isEmpty)
        #expect(NotePolicy.notes(for: change, in: model.store, NotePolicy.Settings()).count == 1)
        var quiet = NotePolicy.Settings()
        quiet.sound = false
        #expect(NotePolicy.notes(for: change, in: model.store, quiet).first?.sound == false)
        await core.close()
    }

    /// Running dry after finishing something is a queue's end; emptied by
    /// removing what waited, it isn't. One download's end says enough.
    @Test func aQueueThatRunsDryAfterFinishingSeveralNotifies() async {
        let (model, notifier, _) = model([download("a", .active), download("b", .active)])
        var changes: [StoreChange] = []
        let handle = model.store.onChange
        model.store.onChange = {
            changes.append($0)
            handle?($0)
        }
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        model.store.apply(.queueChanged(snapshot: queue(queues[0], queued: 0, running: 1)))
        model.store.apply(.stateChanged(id: "b", from: .active, to: .completed, reason: nil))
        model.store.apply(.queueChanged(snapshot: queue(queues[0], queued: 0, running: 0)))
        #expect(changes.last == .queueRanDry(queue(queues[0], queued: 0, running: 0), finished: 2))
        #expect(notifier.notes.map(\.kind) == [.completed, .completed, .queue])

        // Busy again, then emptied by a removal: no end.
        model.store.apply(.queueChanged(snapshot: queue(queues[0], queued: 1, running: 0)))
        model.store.apply(.queueChanged(snapshot: queue(queues[0], queued: 0, running: 0)))
        #expect(notifier.notes.count == 3)
        #expect(changes.count == 3)
        await core.close()
    }

    @Test func onCompleteCountsDownThenActs() async throws {
        let (model, _, system) = model([download("a", .active)], onComplete: .sleep)
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        model.store.apply(
            .queueChanged(snapshot: queue(queues[0], queued: 0, running: 0, onComplete: .sleep)))
        #expect(model.countdown?.action == .sleep)
        #expect(system.done.isEmpty)
        try await until("acted") { !system.done.isEmpty }
        #expect(system.done == ["sleep"])
        #expect(model.countdown == nil)
        await core.close()
    }

    @Test func onCompleteCanBeCancelledAndStopsForNewWork() async throws {
        let (model, notifier, system) = model(
            [download("a", .active), download("b", .paused)], onComplete: .shutdown)
        model.countdownLength = 1000
        // Another queue still at work: no countdown at all.
        model.startCountdown(.shutDown, queue: "q")
        #expect(model.countdown == nil)
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        model.startCountdown(.shutDown, queue: "q")
        #expect(model.countdown != nil)
        // It says so in a notification too.
        #expect(notifier.notes.last?.title == tr("countdown.shutDownNote \("q")"))
        model.cancelCountdown()
        #expect(model.countdown == nil)
        // A download starting meanwhile calls it off.
        model.startCountdown(.shutDown, queue: "q")
        model.store.apply(.stateChanged(id: "b", from: .paused, to: .active, reason: nil))
        try await until("off") { model.countdown == nil }
        #expect(system.done.isEmpty)
        // "Now" acts at once.
        model.store.apply(.stateChanged(id: "b", from: .active, to: .paused, reason: nil))
        model.startCountdown(.shutDown, queue: "q")
        model.finishCountdown()
        #expect(system.done == ["shutDown"])
        #expect(Countdown.action(for: .hibernate) == .sleep)
        #expect(Countdown.action(for: .nothing) == nil)
        await core.close()
    }

    /// Two queues ran dry: shutting down wins over sleeping.
    @Test func shutDownWinsOverSleep() async {
        let (model, _, system) = model([download("a", .completed)])
        model.countdownLength = 1000
        model.startCountdown(.sleep, queue: "one")
        model.startCountdown(.shutDown, queue: "two")
        #expect(model.countdown == Countdown(action: .shutDown, queue: "two", secondsLeft: 1000))
        model.startCountdown(.sleep, queue: "three")
        #expect(model.countdown?.action == .shutDown)
        model.finishCountdown()
        #expect(system.done == ["shutDown"])
        await core.close()
    }

    /// A sleep or shutdown that fails is said, not swallowed.
    @Test func aFailedActionIsSaid() async {
        let (model, notifier, system) = model([download("a", .completed)])
        system.fails = true
        model.countdownLength = 1000
        model.startCountdown(.sleep, queue: "q")
        model.finishCountdown()
        #expect(notifier.notes.last?.title == tr("countdown.sleepFailed"))
        #expect(model.alert?.message == tr("countdown.sleepFailed"))
        await core.close()
    }

    /// A queue emptied by a pause, cancel, removal or move after an earlier
    /// download finished hasn't ended its run: no countdown.
    @Test func emptyingAQueueAfterAFinishDoesNotEndIt() async {
        for empty in ["pause", "cancel", "remove", "move"] {
            let (model, _, _) = model(
                [download("a", .active), download("b", .active)], onComplete: .shutdown)
            let q = queues[0]
            model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
            model.store.apply(
                .queueChanged(snapshot: queue(q, queued: 0, running: 1, onComplete: .shutdown)))
            switch empty {
            case "pause":
                model.store.apply(.stateChanged(id: "b", from: .active, to: .paused, reason: nil))
            case "cancel":
                model.store.apply(
                    .stateChanged(id: "b", from: .active, to: .cancelled, reason: nil))
            case "remove":
                model.store.apply(.removed(id: "b"))
            default:
                // Moved to another queue: no event about the download.
                break
            }
            model.store.apply(
                .queueChanged(snapshot: queue(q, queued: 0, running: 0, onComplete: .shutdown)))
            #expect(model.countdown == nil, "\(empty)")
        }
        await core.close()
    }

    /// A completion that lands while the list is being read again
    /// (reconcile) is still noticed, once.
    @Test func aCompletionDuringAReconcileNotifiesOnce() async {
        let (model, notifier, _) = model([download("a", .active)])
        model.store.beginReconcile()
        model.store.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        var read = download("a", .completed)
        read.queueId = queues[0].id
        model.store.reconcile([read])
        #expect(notifier.notes.map(\.kind) == [.completed])
        await core.close()
    }

    @Test func linksInProseLoseTheirPunctuation() {
        #expect(AddModel.trimmed("(https://x.test/a.iso),") == "https://x.test/a.iso")
        #expect(AddModel.trimmed("<https://x.test/a>") == "https://x.test/a")
        #expect(AddModel.trimmed("https://x.test/Foo_(bar)") == "https://x.test/Foo_(bar)")
        #expect(AddModel.trimmed("https://x.test/a[1].") == "https://x.test/a[1]")
        #expect(AddModel.trimmed("https://x.test/a]") == "https://x.test/a")
        #expect(RowMenu.resumeTitle([]) == tr("menu.resume"))
        #expect(RowMenu.resumeTitle([.failed]) == tr("menu.retry"))
    }

    @Test func theDockShowsTheActiveCountAndTheirProgress() async {
        let s = store(
            [
                download("a", .active, downloaded: 250, total: 1000),
                download("b", .waiting, downloaded: 0, total: 1000),
                download("c", .active, downloaded: 10, total: nil),
                download("d", .completed, downloaded: 1000, total: 1000),
            ], queues: queues, settings: settings)
        let shown = DockTile.Shown(s)
        #expect(shown == DockTile.Shown(active: 3, percent: 12))
        #expect(shown.badge == 3.formatted())
        let idle = DockTile.Shown(
            store([download("d", .completed)], queues: queues, settings: settings))
        #expect(idle.badge == nil && idle.percent == nil)
        await core.close()
    }

    @Test func queuesAreEditedWithTheirSchedules() async throws {
        var draft = QueueDraft()
        draft.name = "Night"
        draft.maxConcurrent = 2
        draft.limitOn = true
        draft.limitKB = 500
        draft.onComplete = .sleep
        draft.scheduled = true
        draft.action = .startAndStopOutside
        draft.windows = [QueueDraft.newWindow()]
        draft.windows[0].days = [.saturday, .monday]
        let q = try core.engine.createQueue(spec: try draft.spec())
        #expect(q.speedLimit == 500_000)
        #expect(q.schedule?.windows.first?.days == [.monday, .saturday])
        #expect(q.onComplete == .sleep)
        // Read back, it's unchanged.
        #expect(!QueueDraft(q).differs(from: q))

        // A window without days: the core refuses it.
        var bad = QueueDraft(q)
        bad.windows[0].days = []
        #expect(bad.differs(from: q))
        #expect(throws: DhowError.self) {
            try core.engine.updateQueue(id: q.id, spec: try bad.spec())
        }
        bad.maxConcurrent = -1
        #expect(throws: SettingsDraft.OutOfRange.self) { try bad.spec() }

        // The default queue keeps the shell's name when left empty.
        var main = QueueDraft(queues[0])
        #expect(main.name.isEmpty && main.isDefault)
        main.onComplete = .shutdown
        main.scheduled = true
        let updated = try core.engine.updateQueue(id: queues[0].id, spec: try main.spec())
        #expect(updated.name == nil)
        #expect(updated.schedule == nil)
        #expect(updated.onComplete == .shutdown)
        await core.close()
    }

    /// "Download with Dhow" (Services): the links selected open Add.
    @Test func linksFromTheServicesMenuOpenAdd() async throws {
        let model = AppModel()
        let other = try TempEngine(listener: model.listener)
        try await model.attach(other.engine)
        let services = Services()
        services.model = model
        let board = NSPasteboard(name: NSPasteboard.Name("dhow-test-\(UUID().uuidString)"))
        board.clearContents()
        board.setString(
            "see https://example.com/a.iso and https://example.com/a.iso, ftp://x", forType: .string
        )
        var error: NSString?
        services.downloadURLs(board, userData: nil, error: &error)
        #expect(model.add?.text == "https://example.com/a.iso")
        board.releaseGlobally()
        await other.close()
        await core.close()
    }

    @Test func logLevelsAreFilters() {
        #expect(LogLevel.standard.filter == "warn,dhow=info")
        #expect(LogLevel.allCases.count == 3)
    }
}
