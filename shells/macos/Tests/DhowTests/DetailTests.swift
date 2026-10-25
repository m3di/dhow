import DhowCore
import Foundation
import Testing

@testable import Dhow

/// The detail pane's data (UI-GUIDELINES §5): its log, segment map watch
/// and speed trail, all kept from events.
@MainActor
@Suite struct DetailTests {
    let core: TempEngine
    let queues: [QueueSnapshot]
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        queues = try core.engine.listQueues()
        settings = try core.engine.settings()
    }

    @Test func theLogKeepsTheLastTwentyEventsOfEachDownload() async {
        let clock = Clock()
        let s = store(
            [download("a"), download("b")], queues: queues, settings: settings, clock: clock)
        s.apply(.stateChanged(id: "a", from: .queued, to: .active, reason: nil))
        clock.now += 5
        s.apply(.warning(id: "a", kind: .notResumable))
        s.apply(.retrying(id: "a", failures: 2, delayMs: 1500))
        s.apply(.reset(id: "a", reason: .serverFileChanged))
        s.apply(.stateChanged(id: "a", from: .active, to: .failed, reason: .noSpace))
        // A warning about no download, and events of unknown ones, are not logged.
        s.apply(.warning(id: nil, kind: .identityWeak))
        s.apply(.warning(id: "x", kind: .identityWeak))
        #expect(
            s.logs["a"]?.map(\.kind) == [
                .state(.active, nil), .warning(.notResumable),
                .retrying(attempt: 2, delayMs: 1500), .reset(.serverFileChanged),
                .state(.failed, .noSpace),
            ])
        #expect(s.logs["a"]?.first?.at == Date(timeIntervalSince1970: 1_000_000))
        #expect(s.logs["a"]?.last?.at == Date(timeIntervalSince1970: 1_000_005))
        #expect(s.logs["b"] == nil)
        #expect(s.logs.keys.sorted() == ["a"])

        for i in 0..<30 {
            s.apply(.retrying(id: "b", failures: UInt32(i), delayMs: 0))
        }
        #expect(s.logs["b"]?.count == DownloadStore.logLength)
        #expect(s.logs["b"]?.first?.kind == .retrying(attempt: 10, delayMs: 0))
        s.apply(.removed(id: "b"))
        #expect(s.logs["b"] == nil)
        await core.close()
    }

    @Test func logLinesReadAsShortPhrases() {
        let at = Date(timeIntervalSince1970: 0)
        let line = LogText.line(LogEntry(id: 1, at: at, kind: .state(.failed, .http(status: 503))))
        #expect(line.hasSuffix("Stopped: Server error 503"))
        #expect(
            LogText.text(.retrying(attempt: 3, delayMs: 2500)).hasPrefix(
                "Reconnecting (attempt 3) in"))
        #expect(LogText.text(.renamed("/tmp/x/movie.mkv")) == "Saved as movie.mkv")
        for kind in WarningKind.allCases {
            #expect(!looksLikeKey(LogText.warning(kind)))
        }
        for reason in ResetReason.allCases {
            #expect(!looksLikeKey(LogText.reset(reason)))
        }
    }

    @Test func rangeMapsComeFromEventsAndGoWithTheDownload() async {
        let s = store([download("a", .active)], queues: queues, settings: settings)
        let map = RangeMap(
            total: 100, done: [ByteRange(start: 0, end: 40)],
            connections: [ConnectionRange(start: 40, at: 50, end: 70)],
            pending: [ByteRange(start: 70, end: 100)])
        s.apply(.rangeMap(id: "a", map: map))
        s.apply(.rangeMap(id: "x", map: map))
        #expect(s.rangeMaps == ["a": map])
        s.apply(.removed(id: "a"))
        #expect(s.rangeMaps.isEmpty)
        await core.close()
    }

    /// On only while the pane shows a running download; one that doesn't
    /// run gets its sidecar's map once, and nothing stays watched.
    @Test func theSegmentMapIsWatchedOnlyWhileTheShownDownloadRuns() {
        typealias C = RangeWatch.Call
        var w = RangeWatch()
        #expect(w.show("a", state: .active) == [C(id: "a", on: true)])
        #expect(w.show("a", state: .active).isEmpty)
        // It stops: off; its run's end sends the last map.
        #expect(w.show("a", state: .paused) == [C(id: "a", on: false)])
        #expect(w.show("a", state: .paused).isEmpty)
        // Another, paused: one map, then off at once.
        #expect(w.show("b", state: .paused) == [C(id: "b", on: true), C(id: "b", on: false)])
        #expect(w.show("b", state: .queued).isEmpty)
        // It starts.
        #expect(w.show("b", state: .active) == [C(id: "b", on: true)])
        // The selection moves to a running one, then the pane closes.
        #expect(w.show("c", state: .waiting) == [C(id: "b", on: false), C(id: "c", on: true)])
        #expect(w.show(nil, state: nil) == [C(id: "c", on: false)])
        #expect(w.watched == nil)
        // Finished ones have no map to show.
        #expect(w.show("d", state: .completed).isEmpty)
        #expect(w.show("e", state: .cancelled).isEmpty)
    }

    @Test func theTrailIsTheHistoryThenASampleASecondFromProgress() async {
        let clock = Clock()
        let s = store([download("a", .active)], queues: queues, settings: settings, clock: clock)
        func progress(_ id: String, _ speed: UInt64) {
            s.apply(
                .progress(
                    id: id, downloaded: 0, total: nil, connections: 1, speedBps: speed,
                    etaSecs: nil))
        }
        progress("a", 1)
        #expect(s.speedTrail.isEmpty)
        s.showTrail("a")
        progress("a", 100)
        progress("a", 200)
        #expect(s.speedTrail.map(\.bps) == [100])
        // History read meanwhile: its samples, then the newer live one.
        let ms = { (d: Date) in Int64(d.timeIntervalSince1970 * 1000) }
        s.setTrail(
            "a",
            history: [
                SpeedSample(at: ms(clock.now) - 700_000, periodSecs: 1, bytesPerSec: 1),
                SpeedSample(at: ms(clock.now) - 3_600_000, periodSecs: 3600, bytesPerSec: 9),
                SpeedSample(at: ms(clock.now) - 2000, periodSecs: 1, bytesPerSec: 50),
            ])
        // The one from 700 s ago is out of the window, the hourly one isn't per second.
        #expect(s.speedTrail.map(\.bps) == [50, 100])
        clock.now += 1
        progress("a", 300)
        #expect(s.speedTrail.map(\.bps) == [50, 100, 300])
        // Stopping draws the drop to 0.
        s.apply(.stateChanged(id: "a", from: .active, to: .paused, reason: nil))
        #expect(s.speedTrail.last?.bps == 0)
        // A stale history (the pane moved on) is ignored.
        s.showTrail("b")
        s.setTrail("a", history: [SpeedSample(at: ms(clock.now), periodSecs: 1, bytesPerSec: 7)])
        #expect(s.speedTrail.isEmpty)
        await core.close()
    }

    @Test func aChangeNoEventReportsIsAdjustedInPlace() async {
        let s = store([download("a", .active)], queues: queues, settings: settings)
        s.adjust("a") { $0.speedLimit = 2048 }
        s.adjust("x") { $0.speedLimit = 1 }
        #expect(s.downloads["a"]?.speedLimit == 2048)
        #expect(s.downloads["a"]?.state == .active)
        #expect(s.downloads["x"] == nil)
        await core.close()
    }
}
