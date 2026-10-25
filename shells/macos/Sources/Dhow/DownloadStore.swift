import DhowCore
import Foundation
import Observation

/// What the core last said about a running download that its snapshot
/// doesn't carry (ARCHITECTURE §3.2): `Progress` rates, and the attempt of
/// the last `Retrying` until the next `Progress`.
struct Live: Equatable {
    var speedBps: UInt64 = 0
    var etaSecs: UInt64?
    var connections: UInt32 = 0
    var reconnecting: UInt32?
}

/// The one source of truth the views render (UI-GUIDELINES §10): the core's
/// snapshots from `list()` at launch, kept current by its events. Main
/// actor only; `Listener` hops events here in order.
@MainActor @Observable
final class DownloadStore {
    private(set) var downloads: [String: DownloadSnapshot] = [:]
    /// `list()`'s order (queue order), then as added.
    private(set) var order: [String] = []
    private(set) var live: [String: Live] = [:]
    /// Default queue first, then by name, as `list_queues`.
    private(set) var queues: [QueueSnapshot] = []
    private(set) var groups: [String: GroupSnapshot] = [:]
    private(set) var settings: DhowSettings?
    private(set) var network: NetworkInfo?
    /// The aggregate speed, at most one sample a second while anything
    /// runs, oldest first: the toolbar's sparkline. Taken as `Progress`
    /// arrives, so nothing ticks when idle.
    private(set) var speedSamples: [UInt64] = []
    private(set) var loaded = false

    nonisolated static let sparklineLength = 60
    /// Events that came before the launch `list()` was in: replayed over
    /// it, as they may be newer than what it read.
    private var early: [Event] = []
    private var lastSample: Date?
    private let now: () -> Date

    init(now: @escaping () -> Date = Date.init) {
        self.now = now
    }

    func load(
        downloads list: [DownloadSnapshot], queues: [QueueSnapshot], groups: [GroupSnapshot],
        settings: DhowSettings
    ) {
        downloads = Dictionary(list.map { ($0.id, $0) }, uniquingKeysWith: { _, b in b })
        order = list.map(\.id)
        self.queues = queues
        self.groups = Dictionary(groups.map { ($0.id, $0) }, uniquingKeysWith: { _, b in b })
        self.settings = settings
        loaded = true
        let replay = early
        early = []
        replay.forEach(apply)
    }

    func apply(_ event: Event) {
        guard loaded else {
            early.append(event)
            return
        }
        switch event {
        case .downloadAdded(let snapshot):
            if downloads[snapshot.id] == nil {
                order.append(snapshot.id)
            }
            downloads[snapshot.id] = snapshot
        case .removed(let id):
            downloads[id] = nil
            live[id] = nil
            order.removeAll { $0 == id }
            sampleSpeed()
        case .stateChanged(let id, _, let to, let reason):
            update(id) {
                $0.state = to
                $0.failReason = reason
            }
            if to != .active {
                live[id] = nil
            }
            sampleSpeed()
        case .progress(let id, let downloaded, let total, let connections, let speed, let eta):
            guard downloads[id] != nil else { return }
            update(id) {
                $0.downloaded = downloaded
                if let total {
                    $0.totalSize = total
                }
            }
            live[id] = Live(speedBps: speed, etaSecs: eta, connections: connections)
            sampleSpeed()
        case .reset(let id, _):
            update(id) { $0.downloaded = 0 }
        case .retrying(let id, let failures, _):
            guard downloads[id] != nil else { return }
            live[id, default: Live()].reconnecting = failures
        case .renamed(let id, let path):
            update(id) { $0.finalPath = path }
        case .groupChanged(let id, let group):
            update(id) { $0.group = group?.id }
            if let group {
                groups[group.id] = group
            }
        case .settingsChanged(let settings):
            self.settings = settings
        case .queueChanged(let snapshot):
            if let i = queues.firstIndex(where: { $0.id == snapshot.id }) {
                queues[i] = snapshot
            } else {
                queues.append(snapshot)
                queues.sort(by: Self.queueOrder)
            }
        case .queueRemoved(let id):
            queues.removeAll { $0.id == id }
        case .networkChanged(let info):
            network = info
        case .warning, .rangeMap:
            // The detail pane's (M4.2, 2/3).
            break
        }
    }

    func queue(_ id: String) -> QueueSnapshot? {
        queues.first { $0.id == id }
    }

    var defaultQueue: QueueSnapshot? {
        queues.first { $0.isDefault }
    }

    /// The sum of the running downloads' speeds.
    var totalSpeed: UInt64 {
        live.reduce(0) { sum, entry in
            downloads[entry.key]?.state == .active ? sum + entry.value.speedBps : sum
        }
    }

    var activeCount: Int {
        downloads.values.count { $0.state.isRunning }
    }

    private func update(_ id: String, _ change: (inout DownloadSnapshot) -> Void) {
        guard var snapshot = downloads[id] else { return }
        change(&snapshot)
        downloads[id] = snapshot
    }

    private func sampleSpeed() {
        guard downloads.values.contains(where: { $0.state == .active }) else {
            speedSamples = []
            lastSample = nil
            return
        }
        let t = now()
        if let last = lastSample, t.timeIntervalSince(last) < 1 {
            return
        }
        lastSample = t
        speedSamples.append(totalSpeed)
        if speedSamples.count > Self.sparklineLength {
            speedSamples.removeFirst(speedSamples.count - Self.sparklineLength)
        }
    }

    private static func queueOrder(_ a: QueueSnapshot, _ b: QueueSnapshot) -> Bool {
        if a.isDefault != b.isDefault {
            return a.isDefault
        }
        return (a.name ?? "") < (b.name ?? "")
    }
}

extension DownloadState {
    /// `Active`, `Waiting` or `Completing`, as `QueueSnapshot.running` counts.
    var isRunning: Bool {
        self == .active || self == .waiting || self == .completing
    }

    var isFinished: Bool {
        self == .completed || self == .cancelled
    }
}
