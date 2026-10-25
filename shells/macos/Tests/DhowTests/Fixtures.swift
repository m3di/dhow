import DhowCore
import Foundation

@testable import Dhow

/// A real engine in a temporary directory, for the core's own records
/// (settings, the default queue) and for calls across the boundary.
final class TempEngine: @unchecked Sendable {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dhow-shell-test-\(UUID().uuidString)")
    let engine: Engine

    init(listener: EventListener = Listener()) throws {
        engine = try Engine.open(
            dataDir: dir.path, config: EngineConfig(), listener: listener, adapter: nil)
    }

    func close() async {
        await engine.shutdown()
        try? FileManager.default.removeItem(at: dir)
    }
}

func download(
    _ id: String, _ state: DownloadState = .queued, name: String? = nil, queue: String = "default",
    downloaded: UInt64 = 0, total: UInt64? = 1000, group: String? = nil,
    category: DhowCore.Category = .other, createdAt: Int64 = 0, speedLimit: UInt64? = nil
) -> DownloadSnapshot {
    DownloadSnapshot(
        id: id, url: "https://example.com/\(name ?? id)", finalPath: "/tmp/\(name ?? id)",
        partPath: "/tmp/\(name ?? id).part", state: state, failReason: nil, totalSize: total,
        downloaded: downloaded, resumable: .yes, sizeUnverified: false, identityWeak: false,
        createdAt: createdAt, queueId: queue, position: 0, speedLimit: speedLimit,
        category: category, group: group, etag: nil, lastModified: nil, connections: nil)
}

/// A clock the test moves.
final class Clock: @unchecked Sendable {
    var now = Date(timeIntervalSince1970: 1_000_000)
}

@MainActor
func store(
    _ downloads: [DownloadSnapshot], queues: [QueueSnapshot], settings: DhowSettings,
    clock: Clock = Clock()
) -> DownloadStore {
    let store = DownloadStore(now: { clock.now })
    store.load(downloads: downloads, queues: queues, groups: [], settings: settings)
    return store
}
