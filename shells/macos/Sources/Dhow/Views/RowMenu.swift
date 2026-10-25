import DhowCore
import SwiftUI

/// The list's context menu (UI-GUIDELINES §3), for one download or several.
struct RowMenu: View {
    @Environment(AppModel.self) private var model
    var ids: Set<String>

    /// Per-download limits offered here; the detail pane has a slider.
    static let limits: [UInt64] = [256, 512, 1000, 2000, 5000, 10_000].map { $0 * 1000 }

    var body: some View {
        let store = model.store
        let selected = ids.compactMap { store.downloads[$0] }
        let states = selected.map(\.state)
        if !selected.isEmpty {
            if states.contains(where: Actions.canResume) {
                Button(Self.resumeTitle(states)) { model.resume(ids) }
            }
            if states.contains(where: Actions.canPause) {
                Button(tr("menu.pause")) { model.pause(ids) }
            }
            Divider()
            if states.contains(.completed) {
                Button(tr("menu.open")) { model.open(ids) }
            }
            Button(tr("menu.showInFinder")) { model.showInFinder(ids) }
            Button(selected.count == 1 ? tr("menu.copyLink") : tr("menu.copyLinks")) {
                model.copyLinks(ids)
            }
            Divider()
            if store.queues.count > 1 {
                Menu(tr("menu.moveToQueue")) {
                    ForEach(store.queues, id: \.id) { q in
                        Toggle(
                            q.title,
                            isOn: Binding(
                                get: { selected.allSatisfy { $0.queueId == q.id } },
                                set: { _ in model.move(ids, toQueue: q.id) }))
                    }
                }
            }
            Menu(tr("menu.speedLimit")) {
                limitItem(tr("menu.noLimit"), nil, selected)
                Divider()
                ForEach(Self.limits, id: \.self) { limit in
                    limitItem(Format.speed(limit), limit, selected)
                }
            }
            Divider()
            if states.contains(where: Actions.canCancel) {
                Button(tr("menu.cancelEllipsis")) { model.confirmation = .cancel(ids) }
            }
            Button(tr("menu.remove")) { model.remove(ids, deleteFiles: false) }
            Button(tr("menu.removeAndDeleteEllipsis")) { model.confirmation = .delete(ids) }
        }
    }

    private func limitItem(_ title: String, _ limit: UInt64?, _ selected: [DownloadSnapshot])
        -> some View
    {
        Toggle(
            title,
            isOn: Binding(
                get: { selected.allSatisfy { $0.speedLimit == limit } },
                set: { _ in model.setSpeedLimit(ids, limit) }))
    }

    static func resumeTitle(_ states: [DownloadState]) -> String {
        guard states.contains(where: Actions.canResume) else { return tr("menu.resume") }
        if states.allSatisfy({ $0 == .failed || !Actions.canResume($0) }) {
            return tr("menu.retry")
        }
        if states.allSatisfy({ $0 == .queued || !Actions.canResume($0) }) {
            return tr("menu.startNow")
        }
        return tr("menu.resume")
    }
}
