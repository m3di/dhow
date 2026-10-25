import DhowCore
import Foundation
import Testing

@testable import Dhow

@MainActor
@Suite struct RowTests {
    let core: TempEngine
    let queue: QueueSnapshot
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        queue = try core.engine.listQueues()[0]
        settings = try core.engine.settings()
    }

    func status(_ d: DownloadSnapshot, live: Live? = nil, queue: QueueSnapshot? = nil) -> String {
        RowStatus.of(d, live: live, queue: queue ?? self.queue, now: .now).text
    }

    @Test func statusPhrases() async {
        #expect(status(download("a", .active), live: Live(etaSecs: 180)) == "3 min left")
        #expect(status(download("a", .active)) == "Downloading")
        #expect(
            status(download("a", .active), live: Live(reconnecting: 3))
                == "Reconnecting (attempt 3)")
        #expect(status(download("a", .waiting)) == "Waiting for a free connection")
        #expect(status(download("a", .paused)) == "Paused")
        var failed = download("a", .failed)
        failed.failReason = .http(status: 404)
        #expect(status(failed) == "Server error 404")
        await core.close()
    }

    @Test func queuedSaysWhatItWaitsFor() async {
        var q = queue
        q.activeNow = true
        q.maxConcurrent = 1
        q.running = 1
        #expect(status(download("a"), queue: q) == "Waiting for 1 download to finish")
        q.maxConcurrent = 3
        q.running = 3
        #expect(status(download("a"), queue: q) == "Waiting for 3 downloads to finish")
        q.running = 1
        #expect(status(download("a"), queue: q) == "Queued")
        q.activeNow = false
        q.nextEdge = Int64(Date.now.timeIntervalSince1970 * 1000) + 60_000
        #expect(status(download("a"), queue: q).hasPrefix("Scheduled for "))
        q.runState = .stopped
        q.runStateUntil = nil
        #expect(status(download("a"), queue: q) == "Queue stopped")
        await core.close()
    }

    @Test func filtersAndSearch() async {
        let s = store(
            [
                download("a", .active, name: "ubuntu.iso", queue: queue.id),
                download("b", .completed, name: "paper.pdf", queue: queue.id, category: .document),
                download("c", .failed, name: "x.zip", queue: queue.id),
                download("d", .paused, name: "y.zip"),
            ], queues: [queue], settings: settings)
        func ids(_ item: SidebarItem, _ search: String = "") -> [String] {
            s.rows(for: item, search: search, now: .now).map(\.id)
        }
        #expect(ids(.filter(.all)) == ["a", "b", "c", "d"])
        #expect(ids(.filter(.active)) == ["a"])
        #expect(ids(.filter(.queued)) == ["d"])
        #expect(ids(.filter(.done)) == ["b"])
        #expect(ids(.filter(.failed)) == ["c"])
        #expect(ids(.category(.document)) == ["b"])
        #expect(ids(.queue(queue.id)) == ["a", "b", "c"])
        #expect(ids(.filter(.all), "ZIP") == ["c", "d"])
        await core.close()
    }

    @Test func groupsAggregateAllTheirMembers() async {
        let s = store(
            [
                download(
                    "p1", .completed, name: "Movie.Part1.rar", downloaded: 100, total: 100,
                    group: "g"),
                download("x", .queued, name: "other"),
                download(
                    "p2", .active, name: "Movie.Part2.rar", downloaded: 50, total: 100, group: "g"),
                download(
                    "p3", .queued, name: "Movie.Part3.rar", downloaded: 0, total: 100, group: "g"),
            ], queues: [queue], settings: settings)
        s.apply(
            .groupChanged(
                id: "p1",
                group: GroupSnapshot(
                    id: "g", name: "Movie.Part", auto: true, members: ["p1", "p2", "p3"],
                    downloaded: 0, total: nil, speedBps: 0, etaSecs: nil)))
        s.apply(
            .progress(
                id: "p2", downloaded: 50, total: 100, connections: 2, speedBps: 2048, etaSecs: 1))

        let rows = s.rows(for: .filter(.all), search: "", now: .now)
        #expect(rows.map(\.id) == ["group:g", "x"])
        let group = rows[0]
        #expect(group.name == "Movie.Part (1 of 3)")
        #expect(group.children?.map(\.id) == ["p1", "p2", "p3"])
        #expect(group.downloadIds == ["p1", "p2", "p3"])
        #expect(group.fraction == 0.5)
        #expect(group.size == 300 && group.sizeKnown)
        #expect(group.speed == 2048)
        // 150 bytes at 2 KiB/s.
        #expect(group.status.text == "1 sec left")

        // A filter shows the group with only the members it matches.
        let active = s.rows(for: .filter(.active), search: "", now: .now)
        #expect(active.map(\.id) == ["group:g"])
        #expect(active[0].children?.map(\.id) == ["p2"])
        #expect(active[0].downloadIds == ["p1", "p2", "p3"])
        await core.close()
    }
}
