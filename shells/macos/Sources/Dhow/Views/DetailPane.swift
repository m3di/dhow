import Charts
import DhowCore
import SwiftUI

/// The inspector (UI-GUIDELINES §5, §8 macOS) for the selection: one
/// download's details, or how many are selected.
struct DetailPane: View {
    @Environment(AppModel.self) private var model
    /// The downloads the selection stands for.
    var ids: Set<String>
    @AppStorage(Prefs.showSegmentMap) private var segmentMap = true

    private var shown: DownloadSnapshot? {
        ids.count == 1 ? ids.first.flatMap { model.store.downloads[$0] } : nil
    }

    var body: some View {
        Group {
            if let d = shown {
                DownloadDetail(download: d)
            } else {
                Text(ids.isEmpty ? tr("detail.none") : tr("detail.many \(ids.count)"))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        // The segment map is watched while this shows a running download.
        .onChange(of: WatchKey(id: shown?.id, state: shown?.state, map: segmentMap), initial: true)
        { _, key in
            model.showDetail(key.id, ranges: key.map)
        }
        .onDisappear { model.showDetail(nil) }
    }

    private struct WatchKey: Equatable {
        var id: String?
        var state: DownloadState?
        var map: Bool
    }
}

private struct DownloadDetail: View {
    @Environment(AppModel.self) private var model
    var download: DownloadSnapshot
    @AppStorage(Prefs.showSegmentMap) private var segmentMap = true

    var body: some View {
        let d = download
        let store = model.store
        Form {
            Section {
                DetailHeader(download: d)
            }
            if !d.state.isFinished {
                Section(tr("detail.progress")) {
                    ProgressSection(download: d, map: segmentMap ? store.rangeMaps[d.id] : nil)
                }
            }
            if d.state != .completed || store.speedTrail.count > 1 {
                Section(tr("detail.speed")) {
                    SpeedChart(points: store.trailId == d.id ? store.speedTrail : [])
                }
            }
            Section(tr("detail.connection")) {
                ConnectionInfo(download: d)
            }
            if !d.state.isFinished {
                Section(tr("detail.controls")) {
                    LimitSlider(download: d)
                    ConnectionsStepper(download: d)
                    HStack {
                        if d.state == .failed {
                            // Back in its queue, or at once by hand.
                            Button(tr("detail.retry")) { model.retry(d.id) }
                            Button(tr("menu.retry")) { model.resume([d.id]) }
                        }
                        Spacer()
                        if Actions.canCancel(d.state) {
                            Button(tr("menu.cancelEllipsis")) {
                                model.confirmation = .cancel([d.id])
                            }
                        }
                    }
                }
            }
            Section(tr("detail.log")) {
                EventLog(entries: store.logs[d.id] ?? [])
            }
        }
        .formStyle(.grouped)
    }
}

/// Name, category, size, where it goes, its queue and group (§5.1).
private struct DetailHeader: View {
    @Environment(AppModel.self) private var model
    var download: DownloadSnapshot

    var body: some View {
        let d = download
        let store = model.store
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: d.category.icon)
                .font(.title)
                .foregroundStyle(.secondary)
                .frame(width: 32)
                .accessibilityLabel(d.category.title)
            VStack(alignment: .leading, spacing: 2) {
                Text(d.fileName)
                    .font(.headline)
                    .lineLimit(3)
                    .textSelection(.enabled)
                Text(size(d))
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
        LabeledContent(tr("detail.saveTo")) {
            Button {
                model.showInFinder([d.id])
            } label: {
                // The folder's name on one line; the whole path on hover.
                Label(Self.folderName(d.finalPath), systemImage: "folder")
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .buttonStyle(.link)
            .help(Self.folder(d.finalPath))
        }
        LabeledContent(tr("detail.queue"), value: store.queue(d.queueId)?.title ?? "")
        if let g = d.group {
            LabeledContent(tr("detail.group"), value: store.groups[g]?.name ?? "")
        }
    }

    private func size(_ d: DownloadSnapshot) -> String {
        guard let total = d.totalSize else {
            return d.state == .completed ? Format.bytes(d.downloaded) : tr("detail.sizeUnknown")
        }
        return Format.bytes(total)
    }

    static func folderName(_ path: String) -> String {
        FileManager.default.displayName(atPath: (path as NSString).deletingLastPathComponent)
    }

    /// The folder, with the home folder as "~".
    static func folder(_ path: String) -> String {
        let dir = (path as NSString).deletingLastPathComponent
        return (dir as NSString).abbreviatingWithTildeInPath
    }
}

/// The segment map and how far along it is (§5.2).
private struct ProgressSection: View {
    var download: DownloadSnapshot
    var map: RangeMap?

    var body: some View {
        let d = download
        let fraction = DownloadStore.fraction(d.downloaded, d.totalSize)
        VStack(alignment: .leading, spacing: 6) {
            if let map, let total = map.total, total > 0 {
                SegmentMap(map: map, total: total)
                    .frame(height: 14)
                    .accessibilityElement()
                    .accessibilityLabel(tr("detail.mapLabel"))
                    .accessibilityValue(fraction.map { $0.formatted(.percent) } ?? "")
                HStack(spacing: 12) {
                    Swatch(title: tr("detail.mapDone"), style: AnyShapeStyle(Color.accentColor))
                    Swatch(
                        title: tr("detail.mapActive"),
                        style: AnyShapeStyle(Color.accentColor.opacity(0.45)))
                    Swatch(title: tr("detail.mapPending"), style: AnyShapeStyle(SegmentMap.empty))
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            } else {
                ProgressView(value: fraction)
                    .progressViewStyle(.linear)
                    .accessibilityValue(fraction.map { $0.formatted(.percent) } ?? "")
            }
            Text(amount(d, fraction))
                .monospacedDigit()
                .foregroundStyle(.secondary)
        }
    }

    private func amount(_ d: DownloadSnapshot, _ fraction: Double?) -> String {
        let done = Format.bytes(d.downloaded)
        guard let total = d.totalSize, let fraction else {
            return tr("detail.amountUnknown \(done)")
        }
        let percent = fraction.formatted(.percent.precision(.fractionLength(0)))
        return tr("detail.amount \(done) \(Format.bytes(total)) \(percent)")
    }
}

private struct Swatch: View {
    var title: String
    var style: AnyShapeStyle

    var body: some View {
        HStack(spacing: 4) {
            RoundedRectangle(cornerRadius: 2).fill(style).frame(width: 10, height: 10)
            Text(title)
        }
    }
}

/// The file as one strip (§5.2): committed ranges filled, each connection's
/// received part lighter up to a tick where it is, the rest empty. Drawn
/// only when a `RangeMap` comes; nothing animates.
struct SegmentMap: View {
    var map: RangeMap
    var total: UInt64

    static let empty = Color.secondary.opacity(0.22)

    var body: some View {
        Canvas { ctx, size in
            let scale = size.width / CGFloat(total)
            func rect(_ start: UInt64, _ end: UInt64) -> CGRect {
                let a = CGFloat(min(start, total)) * scale
                let b = CGFloat(min(end, total)) * scale
                // A range too small to see still shows as a hairline.
                return CGRect(x: a, y: 0, width: max(b - a, 1), height: size.height)
            }
            ctx.fill(Path(CGRect(origin: .zero, size: size)), with: .color(Self.empty))
            for r in map.done where r.end > r.start {
                ctx.fill(Path(rect(r.start, r.end)), with: .color(.accentColor))
            }
            for c in map.connections {
                if c.at > c.start {
                    ctx.fill(
                        Path(rect(c.start, c.at)), with: .color(.accentColor.opacity(0.45)))
                }
                let x = CGFloat(min(c.at, total)) * scale
                ctx.fill(
                    Path(CGRect(x: max(0, x - 1), y: 0, width: 2, height: size.height)),
                    with: .color(.accentColor))
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 3))
    }
}

/// The last ten minutes (§5.3): the y axis scales to the fastest, x labels
/// every 2 minutes. Redrawn as samples come, at most once a second.
private struct SpeedChart: View {
    var points: [SpeedPoint]

    var body: some View {
        if let end = points.last?.at, points.count > 1 {
            Chart(Array(points.enumerated()), id: \.offset) { item in
                AreaMark(
                    x: .value(tr("chart.time"), item.element.at),
                    y: .value(tr("chart.speed"), Double(item.element.bps))
                )
                .foregroundStyle(Color.accentColor.opacity(0.15))
                LineMark(
                    x: .value(tr("chart.time"), item.element.at),
                    y: .value(tr("chart.speed"), Double(item.element.bps))
                )
                .foregroundStyle(Color.accentColor)
                .lineStyle(StrokeStyle(lineWidth: 1.5))
            }
            .chartXScale(domain: end.addingTimeInterval(-DownloadStore.trailWindow)...end)
            // From 0 to the fastest; a flat 0 lies on the floor.
            .chartYScale(domain: 0...max(1000, Double(points.map(\.bps).max() ?? 0)))
            .chartXAxis {
                AxisMarks(values: .stride(by: .minute, count: 2)) { _ in
                    AxisGridLine()
                    // "4:42": the strip has room for five short labels.
                    AxisValueLabel(
                        format: .dateTime.hour(.defaultDigits(amPM: .omitted)).minute())
                }
            }
            .chartYAxis {
                AxisMarks(values: .automatic(desiredCount: 3)) { value in
                    AxisGridLine()
                    AxisValueLabel {
                        if let bps = value.as(Double.self) {
                            Text(Format.speed(UInt64(max(0, bps))))
                        }
                    }
                }
            }
            .frame(height: 110)
            .accessibilityLabel(tr("detail.chartLabel"))
        } else {
            Text(tr("detail.noSpeed"))
                .foregroundStyle(.secondary)
        }
    }
}

/// Connections, resumability, the server and, disclosed, the identity
/// the core checks the file against (§5.4).
private struct ConnectionInfo: View {
    @Environment(AppModel.self) private var model
    var download: DownloadSnapshot
    @State private var identityShown = false

    var body: some View {
        let d = download
        if d.state == .active {
            LabeledContent(
                tr("detail.activeConnections"),
                value: String(model.store.live[d.id]?.connections ?? 0))
        }
        LabeledContent(tr("detail.resumable"), value: resumable(d.resumable))
        LabeledContent(tr("detail.server"), value: URL(string: d.url)?.host() ?? "")
        DisclosureGroup(tr("detail.identity"), isExpanded: $identityShown) {
            LabeledContent(tr("detail.url")) {
                Text(d.url)
                    .lineLimit(4)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
                    .multilineTextAlignment(.trailing)
            }
            LabeledContent(tr("detail.etag")) {
                Text(d.etag ?? tr("detail.notSent")).textSelection(.enabled)
            }
            LabeledContent(tr("detail.lastModified")) {
                Text(d.lastModified ?? tr("detail.notSent")).textSelection(.enabled)
            }
            if d.identityWeak {
                Text(tr("warning.identityWeak")).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func resumable(_ r: Resumable) -> String {
        switch r {
        case .yes: tr("detail.resumableYes")
        case .no: tr("detail.resumableNo")
        case .unknown: tr("detail.resumableUnknown")
        }
    }
}

/// The download's own speed limit (§5.5), applied live once the slider
/// rests; the global limit still applies over it.
private struct LimitSlider: View {
    @Environment(AppModel.self) private var model
    var download: DownloadSnapshot
    /// The limit while the user moves it, until the download has it.
    @State private var editing: PendingLimit?

    struct PendingLimit: Equatable {
        var id: String
        var limit: UInt64?
        var sent = false
    }

    var body: some View {
        let pending = editing?.id == download.id ? editing : nil
        let shown = pending.map(\.limit) ?? download.speedLimit
        let steps = LimitSteps.steps(for: download.speedLimit)
        VStack(alignment: .leading, spacing: 4) {
            LabeledContent(tr("detail.speedLimit")) {
                Text(LimitSteps.label(shown)).monospacedDigit()
            }
            Slider(
                value: Binding(
                    get: { Double(LimitSteps.index(of: shown, in: steps)) },
                    set: {
                        editing = PendingLimit(id: download.id, limit: steps[Int($0.rounded())])
                    }),
                in: 0...Double(steps.count - 1), step: 1
            ) {
                Text(tr("detail.speedLimit"))
            }
            .labelsHidden()
            .accessibilityValue(LimitSteps.label(shown))
        }
        // Applied once it rests, not on every step of a drag.
        .task(id: editing) {
            guard let p = editing, !p.sent else { return }
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            send(p)
        }
        // Kept until the download has it, whatever else changes meanwhile.
        .onChange(of: download.speedLimit) {
            if let p = editing, p.id == download.id, p.limit == download.speedLimit {
                editing = nil
            }
        }
        // Another download, or the pane closes: a change still resting goes now.
        .onChange(of: download.id) { flush() }
        .onDisappear { flush() }
    }

    private func send(_ p: PendingLimit) {
        model.setSpeedLimit([p.id], p.limit)
        editing?.sent = true
    }

    private func flush() {
        if let p = editing, !p.sent {
            model.setSpeedLimit([p.id], p.limit)
        }
        editing = nil
    }
}

/// The detail pane's speed-limit steps (§5.5): kB/s, decimal as `Format`
/// shows them, then no limit. 0 (held by its limit, set elsewhere) is a
/// step only while it is the limit, so the slider and label agree.
enum LimitSteps {
    static let rates: [UInt64] =
        [64, 128, 256, 512, 1000, 2000, 5000, 10_000, 20_000, 50_000].map { $0 * 1000 }

    static func steps(for limit: UInt64?) -> [UInt64?] {
        (limit == 0 ? [0] : []) + rates.map { $0 } + [nil]
    }

    /// The step of a limit, or the nearest (one the CLI set may fall
    /// between).
    static func index(of limit: UInt64?, in steps: [UInt64?]) -> Int {
        if let exact = steps.firstIndex(of: limit) {
            return exact
        }
        guard let limit else { return steps.count - 1 }
        let distance = { (i: Int) -> Double in
            guard let rate = steps[i], rate > 0 else { return .infinity }
            return abs(log2(Double(rate)) - log2(Double(max(limit, 1))))
        }
        return steps.indices.min { distance($0) < distance($1) } ?? 0
    }

    static func label(_ limit: UInt64?) -> String {
        switch limit {
        case nil: tr("menu.noLimit")
        case 0: tr("status.limitedToZero")
        case let rate?: Format.speed(rate)
        }
    }
}

/// Connections for this download (§5.5): a running one follows at once.
/// Steps go to the engine in order; the stepper counts from the last one
/// asked for, not from the snapshot that hasn't caught up yet.
private struct ConnectionsStepper: View {
    @Environment(AppModel.self) private var model
    var download: DownloadSnapshot
    @State private var pending: (id: String, value: Int)?

    var body: some View {
        let settings = model.store.settings
        let standard = Int(settings?.defaultConnections ?? 8)
        let maximum = max(1, Int(settings?.maxConnectionsPerDownload ?? 16))
        let stored = download.connections.map(Int.init) ?? standard
        let current = pending.flatMap { $0.id == download.id ? $0.value : nil } ?? stored
        HStack {
            Stepper(
                value: Binding(
                    get: { current },
                    set: {
                        pending = (download.id, $0)
                        model.setConnections(download.id, UInt8(clamping: $0))
                    }),
                in: 1...max(maximum, current)
            ) {
                Text(
                    download.connections == nil && pending == nil
                        ? tr("detail.connectionsDefault \(current)")
                        : tr("detail.connections \(current)"))
            }
            Spacer()
            if download.connections != nil {
                Button(tr("detail.useDefault")) {
                    pending = nil
                    model.setConnections(download.id, nil)
                }
                .controlSize(.small)
            }
        }
        .onChange(of: download.connections) {
            if let p = pending, p.id == download.id, p.value == download.connections.map(Int.init) {
                pending = nil
            }
        }
        .onChange(of: download.id) { pending = nil }
    }
}

/// The last 20 events for this download (§5.6): plain, selectable text.
private struct EventLog: View {
    var entries: [LogEntry]

    var body: some View {
        if entries.isEmpty {
            Text(tr("detail.logEmpty")).foregroundStyle(.secondary)
        } else {
            Text(entries.map(LogText.line).joined(separator: "\n"))
                .font(.callout)
                .monospacedDigit()
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}
