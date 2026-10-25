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

/// One line of the detail pane's log (UI-GUIDELINES §5.6), stamped when it
/// arrives: events carry no time (DECISIONS 2026-10-25 "`QueueChanged`").
struct LogEntry: Identifiable, Equatable {
    enum Kind: Equatable {
        case added
        case state(DownloadState, FailReason?)
        case retrying(attempt: UInt32, delayMs: UInt64)
        case reset(ResetReason)
        case warning(WarningKind)
        case renamed(String)
    }

    var id: Int
    var at: Date
    var kind: Kind
}

/// A point of the detail pane's speed chart.
struct SpeedPoint: Equatable {
    var at: Date
    var bps: UInt64
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
    /// The detail pane's log per download: the last `logLength` events
    /// since launch, oldest first.
    private(set) var logs: [String: [LogEntry]] = [:]
    /// The last segment map of each download watched (`watch_ranges`).
    private(set) var rangeMaps: [String: RangeMap] = [:]
    /// The download the detail pane shows, and its speed over the last
    /// `trailWindow`: `speed_history`, then a sample a second from its
    /// `Progress` (the chart, UI-GUIDELINES §5.3).
    private(set) var trailId: String?
    private(set) var speedTrail: [SpeedPoint] = []

    nonisolated static let sparklineLength = 60
    nonisolated static let logLength = 20
    nonisolated static let trailWindow: TimeInterval = 600
    /// Asked for when a download names a group the store hasn't seen.
    @ObservationIgnored var unknownGroup: (() -> Void)?
    /// Events that came while a `list()` was being read (at launch, or
    /// reconciling): replayed over it, as they may be newer than it.
    private var early: [Event] = []
    /// `list()` reads in flight.
    private var reconciling = 0
    private var lastSample: Date?
    private var logSeq = 0
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
        replayEarly()
    }

    /// Before reading `list()` again: events wait until `reconcile`.
    func beginReconcile() {
        reconciling += 1
    }

    /// The known downloads as `list` read them, then the events that came
    /// meanwhile. New and removed ones come as events (`recheck`).
    func reconcile(_ list: [DownloadSnapshot]?) {
        for snapshot in list ?? [] where downloads[snapshot.id] != nil {
            downloads[snapshot.id] = snapshot
        }
        reconciling -= 1
        if reconciling == 0 {
            replayEarly()
        }
    }

    /// What a command changed that no event reports (a speed limit, the
    /// connections, the queue): only that field, as later events may
    /// already have changed the rest.
    func adjust(_ id: String, _ change: (inout DownloadSnapshot) -> Void) {
        update(id, change)
    }

    func setNetwork(_ info: NetworkInfo) {
        network = info
    }

    func setGroups(_ list: [GroupSnapshot]) {
        groups = Dictionary(list.map { ($0.id, $0) }, uniquingKeysWith: { _, b in b })
    }

    private func replayEarly() {
        let replay = early
        early = []
        for event in replay {
            apply(event)
        }
    }

    func apply(_ event: Event) {
        guard loaded, reconciling == 0 else {
            early.append(event)
            return
        }
        switch event {
        case .downloadAdded(let snapshot):
            if downloads[snapshot.id] == nil {
                order.append(snapshot.id)
            }
            downloads[snapshot.id] = snapshot
            log(snapshot.id, .added)
            if let g = snapshot.group, groups[g] == nil {
                unknownGroup?()
            }
        case .removed(let id):
            downloads[id] = nil
            live[id] = nil
            logs[id] = nil
            rangeMaps[id] = nil
            order.removeAll { $0 == id }
            sampleSpeed()
        case .stateChanged(let id, _, let to, let reason):
            if downloads[id] != nil {
                log(id, .state(to, reason))
            }
            // The chart drops to 0 when it stops.
            if id == trailId, to != .active, downloads[id]?.state == .active {
                appendTrail(0, force: true)
            }
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
            if id == trailId {
                appendTrail(speed)
            }
            sampleSpeed()
        case .reset(let id, let reason):
            guard downloads[id] != nil else { return }
            log(id, .reset(reason))
            update(id) { $0.downloaded = 0 }
        case .retrying(let id, let failures, let delay):
            guard downloads[id] != nil else { return }
            log(id, .retrying(attempt: failures, delayMs: delay))
            live[id, default: Live()].reconnecting = failures
        case .renamed(let id, let path):
            guard downloads[id] != nil else { return }
            log(id, .renamed(path))
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
        case .warning(let id, let kind):
            // One without a download (none yet) has no pane to show in.
            guard let id, downloads[id] != nil else { return }
            log(id, .warning(kind))
        case .rangeMap(let id, let map):
            guard downloads[id] != nil else { return }
            rangeMaps[id] = map
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

    /// The pane now shows `id` (or nothing); its trail is empty until
    /// `setTrail` brings the history.
    func showTrail(_ id: String?) {
        guard id != trailId else { return }
        trailId = id
        speedTrail = []
    }

    /// `speed_history`'s per-second samples, then those `Progress` brought
    /// after them.
    func setTrail(_ id: String, history: [SpeedSample]) {
        guard id == trailId else { return }
        var points = history.filter { $0.periodSecs == 1 }.map {
            SpeedPoint(
                at: Date(timeIntervalSince1970: TimeInterval($0.at) / 1000), bps: $0.bytesPerSec)
        }
        let last = points.last?.at ?? .distantPast
        points += speedTrail.filter { $0.at > last }
        speedTrail = points
        trimTrail()
    }

    private func appendTrail(_ bps: UInt64, force: Bool = false) {
        let t = now()
        if !force, let last = speedTrail.last, t.timeIntervalSince(last.at) < 1 {
            return
        }
        speedTrail.append(SpeedPoint(at: t, bps: bps))
        trimTrail()
    }

    private func trimTrail() {
        guard let newest = speedTrail.last?.at else { return }
        speedTrail.removeAll { newest.timeIntervalSince($0.at) > Self.trailWindow }
    }

    private func log(_ id: String, _ kind: LogEntry.Kind) {
        logSeq += 1
        var entries = logs[id, default: []]
        entries.append(LogEntry(id: logSeq, at: now(), kind: kind))
        if entries.count > Self.logLength {
            entries.removeFirst(entries.count - Self.logLength)
        }
        logs[id] = entries
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
