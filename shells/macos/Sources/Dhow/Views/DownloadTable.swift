import DhowCore
import SwiftUI

/// A destructive action waiting for the user's yes.
enum Confirmation {
    /// Remove and Delete File.
    case delete(Set<String>)
    /// Cancel Download, which deletes the part file.
    case cancel(Set<String>)
}

/// The list (UI-GUIDELINES §3, §8 macOS): a native `Table`, sortable by
/// name, status, size, speed and date added; groups are disclosure rows.
/// With no sort chosen it keeps queue order.
struct DownloadTable: View {
    @Environment(AppModel.self) private var model
    var rows: [ListRow]
    @Binding var selection: Set<ListRow.ID>
    @State private var sortOrder: [KeyPathComparator<ListRow>] = []
    @State private var expanded = Set<ListRow.ID>()
    @State private var confirmation: Confirmation?

    var body: some View {
        Table(of: ListRow.self, selection: $selection, sortOrder: $sortOrder) {
            TableColumn(tr("column.name"), value: \.name) { row in
                Label {
                    HStack(spacing: 4) {
                        Text(row.name).lineLimit(1).truncationMode(.middle)
                        if let detail = row.detail {
                            Text(detail).foregroundStyle(.secondary).fixedSize()
                        }
                    }
                } icon: {
                    Image(systemName: row.isGroup ? "square.stack" : row.category.icon)
                }
            }
            // The flexible one: the others stop growing.
            .width(min: 140, ideal: 260)
            TableColumn(tr("column.status"), value: \.statusRank) { row in
                Label(row.status.text, systemImage: row.status.glyph.rawValue)
                    .lineLimit(1)
                    .foregroundStyle(row.status.glyph == .done ? .secondary : .primary)
            }
            .width(min: 100, ideal: 170, max: 260)
            TableColumn(tr("column.progress")) { row in
                if row.showsProgress {
                    ProgressView(value: row.fraction)
                        .progressViewStyle(.linear)
                        .controlSize(.small)
                        .accessibilityValue(row.fraction.map { $0.formatted(.percent) } ?? "")
                }
            }
            .width(min: 50, ideal: 90, max: 160)
            TableColumn(tr("column.size"), value: \.size) { row in
                Text(row.sizeKnown ? Format.bytes(row.size) : tr("size.unknown"))
                    .monospacedDigit()
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 50, ideal: 72, max: 100)
            TableColumn(tr("column.speed"), value: \.speed) { row in
                Text(row.speed > 0 ? Format.speed(row.speed) : "")
                    .monospacedDigit()
                    .lineLimit(1)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 50, ideal: 82, max: 110)
            TableColumn(tr("column.added"), value: \.added) { row in
                Text(Format.added(unixMs: row.added, now: .now))
                    .lineLimit(1)
                    .foregroundStyle(.secondary)
            }
            .width(min: 70, ideal: 96, max: 130)
        } rows: {
            ForEach(sorted(rows)) { row in
                if let children = row.children {
                    DisclosureTableRow(row, isExpanded: expansion(row.id)) {
                        ForEach(sorted(children)) { TableRow($0) }
                    }
                } else {
                    TableRow(row)
                }
            }
        }
        .contextMenu(forSelectionType: ListRow.ID.self) { ids in
            RowMenu(ids: downloads(ids), confirmation: $confirmation)
        } primaryAction: { ids in
            // A group opens or closes; a download opens its file.
            if ids.count == 1, let id = ids.first,
                rows.contains(where: { $0.id == id && $0.isGroup })
            {
                expanded.formSymmetricDifference([id])
            } else {
                model.open(downloads(ids))
            }
        }
        .confirmationDialog(
            confirmTitle, isPresented: confirmShown, presenting: confirmation
        ) { c in
            switch c {
            case .delete(let ids):
                Button(tr("menu.removeAndDelete"), role: .destructive) {
                    model.remove(ids, deleteFiles: true)
                }
            case .cancel(let ids):
                Button(tr("menu.cancel"), role: .destructive) { model.cancel(ids) }
            }
        } message: { c in
            switch c {
            case .delete: Text(tr("confirm.deleteMessage"))
            case .cancel: Text(tr("confirm.cancelMessage"))
            }
        }
        .onDeleteCommand { model.remove(downloads(selection), deleteFiles: false) }
    }

    private func sorted(_ rows: [ListRow]) -> [ListRow] {
        sortOrder.isEmpty ? rows : rows.sorted(using: sortOrder)
    }

    private func expansion(_ id: ListRow.ID) -> Binding<Bool> {
        Binding(
            get: { expanded.contains(id) },
            set: { if $0 { expanded.insert(id) } else { expanded.remove(id) } })
    }

    /// Rows as downloads: a group stands for the members it shows.
    private func downloads(_ ids: Set<ListRow.ID>) -> Set<String> {
        var out = Set<String>()
        func visit(_ rows: [ListRow]) {
            for row in rows {
                if ids.contains(row.id) {
                    out.formUnion(row.downloadIds)
                }
                visit(row.children ?? [])
            }
        }
        visit(rows)
        return out
    }

    private var confirmTitle: String {
        switch confirmation {
        case .cancel: tr("confirm.cancelTitle")
        case .delete, nil: tr("confirm.deleteTitle")
        }
    }

    private var confirmShown: Binding<Bool> {
        Binding(get: { confirmation != nil }, set: { if !$0 { confirmation = nil } })
    }
}
