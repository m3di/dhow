import DhowCore
import Foundation

/// The sidebar's smart filters (UI-GUIDELINES §3).
enum SmartFilter: String, CaseIterable, Hashable {
    case all, active, queued, done, failed

    func matches(_ state: DownloadState) -> Bool {
        switch self {
        case .all: true
        case .active: state.isRunning
        // Waiting their turn, or on the user.
        case .queued: state == .queued || state == .paused
        case .done: state == .completed
        case .failed: state == .failed
        }
    }
}

enum SidebarItem: Hashable {
    case filter(SmartFilter)
    case queue(String)
    case category(DhowCore.Category)

    func matches(_ d: DownloadSnapshot) -> Bool {
        switch self {
        case .filter(let f): f.matches(d.state)
        case .queue(let id): d.queueId == id
        case .category(let c): d.category == c
        }
    }
}

/// One row of the list: a download, or a group with its members as
/// children (UI-GUIDELINES §3). Sort keys are plain values for `Table`.
struct ListRow: Identifiable, Hashable {
    var id: String
    /// The downloads it stands for: itself, or a group's members (all of
    /// them, also those the filter hides), for actions.
    var downloadIds: [String]
    var name: String
    var category: DhowCore.Category
    var status: RowStatus
    /// 0...1; `nil` while the size is unknown.
    var fraction: Double?
    var showsProgress: Bool
    var size: UInt64
    var sizeKnown: Bool
    var speed: UInt64
    var added: Int64
    var children: [ListRow]?

    var statusRank: Int { status.rank }
    var isGroup: Bool { children != nil }

    static func groupId(_ group: String) -> String { "group:" + group }
}

extension DownloadSnapshot {
    var fileName: String {
        (finalPath as NSString).lastPathComponent
    }
}

extension DownloadStore {
    /// The list for a sidebar item and search text, in list order; a group
    /// shows when a member matches, at its first member's place, with the
    /// members that match.
    func rows(for item: SidebarItem, search: String, now: Date) -> [ListRow] {
        let search = search.trimmingCharacters(in: .whitespaces)
        let matching = order.compactMap { downloads[$0] }.filter { d in
            item.matches(d)
                && (search.isEmpty || d.fileName.localizedStandardContains(search)
                    || d.url.localizedStandardContains(search))
        }
        var rows: [ListRow] = []
        var groupRow: [String: Int] = [:]
        for d in matching {
            guard let g = d.group else {
                rows.append(row(d, now: now))
                continue
            }
            if let i = groupRow[g] {
                rows[i].children?.append(row(d, now: now))
            } else {
                groupRow[g] = rows.count
                rows.append(self.groupRow(g, first: row(d, now: now), now: now))
            }
        }
        return rows
    }

    func row(_ d: DownloadSnapshot, now: Date) -> ListRow {
        let live = live[d.id]
        return ListRow(
            id: d.id, downloadIds: [d.id], name: d.fileName, category: d.category,
            status: RowStatus.of(d, live: live, queue: queue(d.queueId), now: now),
            fraction: Self.fraction(d.downloaded, d.totalSize),
            showsProgress: !d.state.isFinished,
            size: d.totalSize ?? d.downloaded,
            sizeKnown: d.totalSize != nil || d.state == .completed,
            speed: d.state == .active ? live?.speedBps ?? 0 : 0, added: d.createdAt, children: nil)
    }

    /// Aggregates over its members, whatever the filter shows, except
    /// cancelled ones (as the core's group snapshot, ENGINE-SPEC §13.2).
    private func groupRow(_ g: String, first: ListRow, now: Date) -> ListRow {
        let all = order.compactMap { downloads[$0] }.filter { $0.group == g }
        let members = all.filter { $0.state != .cancelled }
        let rows = members.map { row($0, now: now) }
        let done = members.count { $0.state == .completed }
        let sizes = members.compactMap(\.totalSize)
        let total = sizes.count == members.count ? sizes.reduce(0, +) : nil
        let downloaded = members.reduce(0) { $0 + $1.downloaded }
        let speed = rows.reduce(0) { $0 + $1.speed }
        let name = groups[g]?.name ?? first.name
        return ListRow(
            id: ListRow.groupId(g), downloadIds: all.map(\.id),
            name: tr("group.name \(name) \(done) \(members.count)"), category: first.category,
            status: Self.groupStatus(
                rows, remaining: total.map { $0 - min($0, downloaded) },
                speed: speed),
            fraction: Self.fraction(downloaded, total),
            showsProgress: done < members.count, size: total ?? downloaded,
            sizeKnown: total != nil, speed: speed,
            added: all.map(\.createdAt).min() ?? first.added, children: [first])
    }

    /// The most pressing member's status; while members run, the group's
    /// time left from its total speed.
    static func groupStatus(_ members: [ListRow], remaining: UInt64?, speed: UInt64)
        -> RowStatus
    {
        guard var top = members.min(by: { $0.status.rank < $1.status.rank })?.status else {
            return RowStatus(glyph: .done, text: tr("status.done"), rank: 7)
        }
        if top.glyph == .downloading {
            if let remaining, speed >= 1024 {
                let secs = (remaining + speed - 1) / speed
                top.text = tr("status.left \(Format.duration(seconds: secs))")
            } else {
                top.text = tr("status.downloading")
            }
        }
        return top
    }

    static func fraction(_ downloaded: UInt64, _ total: UInt64?) -> Double? {
        guard let total, total > 0 else { return nil }
        return min(1, Double(downloaded) / Double(total))
    }
}
