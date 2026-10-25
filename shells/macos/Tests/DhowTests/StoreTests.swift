import DhowCore
import Foundation
import Testing

@testable import Dhow

@MainActor
@Suite struct StoreTests {
    let core: TempEngine
    let queues: [QueueSnapshot]
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        queues = try core.engine.listQueues()
        settings = try core.engine.settings()
    }

    var defaultQueue: QueueSnapshot { queues[0] }

    @Test func eventsBeforeTheLaunchListAreReplayedOverIt() async {
        let s = DownloadStore()
        // Newer than what `list()` read: the run started meanwhile.
        s.apply(.stateChanged(id: "a", from: .queued, to: .active, reason: nil))
        s.apply(.downloadAdded(snapshot: download("b")))
        #expect(s.downloads.isEmpty)
        s.load(downloads: [download("a")], queues: queues, groups: [], settings: settings)
        #expect(s.downloads["a"]?.state == .active)
        #expect(s.order == ["a", "b"])
        await core.close()
    }

    /// An add the launch `list()` already saw: its `DownloadAdded` (older)
    /// replays, then the events after it, and the row isn't doubled.
    @Test func replayingAnAddTheListAlreadyHasKeepsOneNewestRow() async {
        let s = DownloadStore()
        s.apply(.downloadAdded(snapshot: download("a")))
        s.apply(.stateChanged(id: "a", from: .queued, to: .active, reason: nil))
        s.load(
            downloads: [download("x"), download("a", .active)], queues: queues, groups: [],
            settings: settings)
        #expect(s.order == ["x", "a"])
        #expect(s.downloads["a"]?.state == .active)
        await core.close()
    }

    /// Re-reading the list (the app became active) takes what the CLI
    /// changed; events that came while it was read still win.
    @Test func reconcileTakesTheListThenTheEventsThatCameMeanwhile() async {
        let s = store([download("a", .active), download("b")], queues: queues, settings: settings)
        s.beginReconcile()
        s.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        #expect(s.downloads["a"]?.state == .active)
        // Read before the completion; `dhow pause` paused b meanwhile.
        s.reconcile([download("a", .active), download("b", .paused), download("new")])
        #expect(s.downloads["a"]?.state == .completed)
        #expect(s.downloads["b"]?.state == .paused)
        // New ones come as `DownloadAdded` from `recheck`.
        #expect(s.downloads["new"] == nil)
        await core.close()
    }

    @Test func aDownloadInAnUnknownGroupAsksForTheGroups() async {
        let s = store([], queues: queues, settings: settings)
        var asked = 0
        s.unknownGroup = { asked += 1 }
        s.apply(.downloadAdded(snapshot: download("a", group: "g")))
        #expect(asked == 1)
        s.setGroups([
            GroupSnapshot(
                id: "g", name: "G", auto: true, members: ["a"], downloaded: 0, total: nil,
                speedBps: 0, etaSecs: nil)
        ])
        s.apply(.downloadAdded(snapshot: download("b", group: "g")))
        #expect(asked == 1)
        await core.close()
    }

    @Test func progressAndStateKeepSnapshotsCurrent() async {
        let s = store([download("a", .active)], queues: queues, settings: settings)
        s.apply(
            .progress(
                id: "a", downloaded: 400, total: 2000, connections: 3, speedBps: 5000, etaSecs: 9))
        #expect(s.downloads["a"]?.downloaded == 400)
        #expect(s.downloads["a"]?.totalSize == 2000)
        #expect(s.live["a"]?.speedBps == 5000)
        #expect(s.totalSpeed == 5000)

        s.apply(.retrying(id: "a", failures: 3, delayMs: 1000))
        #expect(s.live["a"]?.reconnecting == 3)
        // "Reconnecting (n)" lasts until the next Progress.
        s.apply(
            .progress(
                id: "a", downloaded: 500, total: 2000, connections: 1, speedBps: 10, etaSecs: nil))
        #expect(s.live["a"]?.reconnecting == nil)

        s.apply(.reset(id: "a", reason: .serverFileChanged))
        #expect(s.downloads["a"]?.downloaded == 0)
        s.apply(.stateChanged(id: "a", from: .active, to: .completed, reason: nil))
        #expect(s.downloads["a"]?.state == .completed)
        #expect(s.live["a"] == nil)
        s.apply(.renamed(id: "a", path: "/tmp/a (1)"))
        #expect(s.downloads["a"]?.fileName == "a (1)")
        s.apply(.removed(id: "a"))
        #expect(s.downloads.isEmpty && s.order.isEmpty)
        // Events for downloads it doesn't know change nothing.
        s.apply(
            .progress(id: "x", downloaded: 1, total: nil, connections: 1, speedBps: 1, etaSecs: nil)
        )
        #expect(s.live.isEmpty)
        await core.close()
    }

    @Test func queuesStayDefaultFirstThenByName() async throws {
        let s = store([], queues: queues, settings: settings)
        let night = try core.engine.createQueue(
            spec: QueueSpec(name: "night", schedule: nil, onComplete: .nothing))
        let bulk = try core.engine.createQueue(
            spec: QueueSpec(name: "bulk", schedule: nil, onComplete: .nothing))
        s.apply(.queueChanged(snapshot: night))
        s.apply(.queueChanged(snapshot: bulk))
        #expect(s.queues.map(\.name) == [nil, "bulk", "night"])
        var changed = night
        changed.running = 2
        s.apply(.queueChanged(snapshot: changed))
        #expect(s.queue(night.id)?.running == 2)
        s.apply(.queueRemoved(id: bulk.id))
        #expect(s.queues.map(\.id) == [defaultQueue.id, night.id])
        await core.close()
    }

    @Test func sparklineSamplesAtMostOnceASecondAndClearsWhenIdle() async {
        let clock = Clock()
        let s = store([download("a", .active)], queues: queues, settings: settings, clock: clock)
        func progress(_ speed: UInt64) {
            s.apply(
                .progress(
                    id: "a", downloaded: 0, total: nil, connections: 1, speedBps: speed,
                    etaSecs: nil))
        }
        progress(100)
        progress(200)
        #expect(s.speedSamples == [100])
        clock.now += 1
        progress(300)
        #expect(s.speedSamples == [100, 300])
        for _ in 0..<100 {
            clock.now += 1
            progress(1)
        }
        #expect(s.speedSamples.count == DownloadStore.sparklineLength)
        s.apply(.stateChanged(id: "a", from: .active, to: .paused, reason: nil))
        #expect(s.speedSamples.isEmpty)
        await core.close()
    }

    @Test func listenerDeliversEngineEventsToTheStoreInOrder() async throws {
        let listener = Listener()
        let core = try TempEngine(listener: listener)
        let s = DownloadStore()
        listener.store = s
        s.load(
            downloads: [], queues: try core.engine.listQueues(), groups: [],
            settings: try core.engine.settings())
        var patch = SettingsPatch()
        patch.maxRetries = 3
        _ = try core.engine.setSettings(patch: patch)
        patch.maxRetries = 4
        _ = try core.engine.setSettings(patch: patch)
        for _ in 0..<200 where s.settings?.maxRetries != 4 {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(s.settings?.maxRetries == 4)
        await core.close()
        await self.core.close()
    }
}
