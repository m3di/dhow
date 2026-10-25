import DhowCore
import SwiftUI

/// The list (UI-GUIDELINES §3, §8 macOS): a native `Table`, sortable by
/// name, status, size, speed and date added; groups are disclosure rows.
/// With no sort chosen it keeps queue order.
struct DownloadTable: View {
    @Environment(AppModel.self) private var model
    var rows: [ListRow]
    @Binding var selection: Set<ListRow.ID>
    @State private var sortOrder: [KeyPathComparator<ListRow>] = []
    @State private var confirmDelete: Set<String>?

    var body: some View {
        Table(of: ListRow.self, selection: $selection, sortOrder: $sortOrder) {
            TableColumn(tr("column.name"), value: \.name) { row in
                Label {
                    Text(row.name).lineLimit(1).truncationMode(.middle)
                } icon: {
                    Image(systemName: row.isGroup ? "square.stack" : row.category.icon)
                }
            }
            .width(min: 140, ideal: 240)
            TableColumn(tr("column.status"), value: \.statusRank) { row in
                Label(row.status.text, systemImage: row.status.glyph.rawValue)
                    .lineLimit(1)
                    .foregroundStyle(row.status.glyph == .done ? .secondary : .primary)
            }
            .width(min: 100, ideal: 160)
            TableColumn(tr("column.progress")) { row in
                if row.showsProgress {
                    ProgressView(value: row.fraction)
                        .progressViewStyle(.linear)
                        .controlSize(.small)
                        .accessibilityValue(row.fraction.map { $0.formatted(.percent) } ?? "")
                }
            }
            .width(min: 50, ideal: 90)
            TableColumn(tr("column.size"), value: \.size) { row in
                Text(row.sizeKnown ? Format.bytes(row.size) : tr("size.unknown"))
                    .monospacedDigit()
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 50, ideal: 70)
            TableColumn(tr("column.speed"), value: \.speed) { row in
                Text(row.speed > 0 ? Format.speed(row.speed) : "")
                    .monospacedDigit()
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 50, ideal: 80)
            TableColumn(tr("column.added"), value: \.added) { row in
                Text(Format.added(unixMs: row.added, now: .now))
                    .foregroundStyle(.secondary)
            }
            .width(min: 60, ideal: 80)
        } rows: {
            ForEach(sorted(rows)) { row in
                if let children = row.children {
                    DisclosureTableRow(row) {
                        ForEach(sorted(children)) { TableRow($0) }
                    }
                } else {
                    TableRow(row)
                }
            }
        }
        .contextMenu(forSelectionType: ListRow.ID.self) { ids in
            RowMenu(ids: downloads(ids), confirmDelete: $confirmDelete)
        } primaryAction: { ids in
            model.open(downloads(ids))
        }
        .confirmationDialog(
            tr("confirm.deleteTitle"), isPresented: confirmShown, presenting: confirmDelete
        ) { ids in
            Button(tr("menu.removeAndDelete"), role: .destructive) {
                model.remove(ids, deleteFiles: true)
            }
        } message: { _ in
            Text(tr("confirm.deleteMessage"))
        }
        .onDeleteCommand { model.remove(downloads(selection), deleteFiles: false) }
    }

    private func sorted(_ rows: [ListRow]) -> [ListRow] {
        sortOrder.isEmpty ? rows : rows.sorted(using: sortOrder)
    }

    /// Rows as downloads: a group stands for its members.
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

    private var confirmShown: Binding<Bool> {
        Binding(get: { confirmDelete != nil }, set: { if !$0 { confirmDelete = nil } })
    }
}
