import DhowCore
import SwiftUI

/// Smart filters, then queues, then categories (UI-GUIDELINES §3).
struct Sidebar: View {
    @Environment(AppModel.self) private var model
    @Binding var selection: SidebarItem?

    var body: some View {
        let store = model.store
        let all = Array(store.downloads.values)
        List(selection: $selection) {
            Section {
                ForEach(SmartFilter.allCases, id: \.self) { f in
                    Label(f.title, systemImage: f.icon)
                        .badge(f == .all ? 0 : all.count { f.matches($0.state) })
                        .tag(SidebarItem.filter(f))
                }
            }
            Section(tr("sidebar.queues")) {
                ForEach(store.queues, id: \.id) { q in
                    Label(q.title, systemImage: "tray.full")
                        .badge(Int(q.unfinished))
                        .tag(SidebarItem.queue(q.id))
                }
            }
            let present = DhowCore.Category.allCases.filter { c in
                all.contains { $0.category == c }
            }
            if !present.isEmpty {
                Section(tr("sidebar.categories")) {
                    ForEach(present, id: \.self) { c in
                        Label(c.title, systemImage: c.icon)
                            .tag(SidebarItem.category(c))
                    }
                }
            }
            // The footer (UI-GUIDELINES §6).
            Section {
                Label(tr("sidebar.storage"), systemImage: "internaldrive")
                    .tag(SidebarItem.storage)
            }
        }
    }
}

extension SmartFilter {
    var title: String {
        switch self {
        case .all: tr("filter.all")
        case .active: tr("filter.active")
        case .queued: tr("filter.queued")
        case .done: tr("filter.done")
        case .failed: tr("filter.failed")
        }
    }

    var icon: String {
        switch self {
        case .all: "tray"
        case .active: "arrow.down.circle"
        case .queued: "hourglass"
        case .done: "checkmark.circle"
        case .failed: "exclamationmark.triangle"
        }
    }
}

extension QueueSnapshot {
    /// The default queue is named by the shell until renamed.
    var title: String { name ?? tr("queue.default") }
}

extension DhowCore.Category {
    var title: String {
        switch self {
        case .video: tr("category.video")
        case .audio: tr("category.audio")
        case .image: tr("category.image")
        case .document: tr("category.document")
        case .archive: tr("category.archive")
        case .software: tr("category.software")
        case .code: tr("category.code")
        case .data: tr("category.data")
        case .other: tr("category.other")
        }
    }

    var icon: String {
        switch self {
        case .video: "film"
        case .audio: "music.note"
        case .image: "photo"
        case .document: "doc.text"
        case .archive: "archivebox"
        case .software: "shippingbox"
        case .code: "chevron.left.forwardslash.chevron.right"
        case .data: "tablecells"
        case .other: "doc"
        }
    }
}
