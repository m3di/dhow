import AppKit
import DhowCore
import SwiftUI

/// Completed downloads by category (UI-GUIDELINES §6): one stacked bar,
/// then the categories with size and count (each filters the list), then
/// the files Dhow can no longer find, which can be forgotten.
struct StorageView: View {
    @Environment(AppModel.self) private var model
    @Binding var sidebar: SidebarItem?
    @State private var usage: [CategoryUsage]?

    /// Read again when a download completes, moves or goes, and when the
    /// app becomes active (files may have moved meanwhile); never on a timer.
    private struct ReloadKey: Equatable {
        var completed: Set<String>
        var activations: Int
    }

    private var reloadKey: ReloadKey {
        ReloadKey(
            completed: Set(
                model.store.downloads.values.filter { $0.state == .completed }.map {
                    $0.id + "\u{0}" + $0.finalPath
                }), activations: model.activations)
    }

    var body: some View {
        Group {
            if let usage {
                if usage.isEmpty {
                    Text(tr("storage.empty"))
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    content(usage)
                }
            } else {
                ProgressView().controlSize(.small)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .navigationTitle(tr("sidebar.storage"))
        .task(id: reloadKey) {
            // A read a newer one superseded (a Forget meanwhile) is dropped.
            if let read = await model.storageBreakdown(), !Task.isCancelled {
                usage = read
            }
        }
    }

    private func content(_ usage: [CategoryUsage]) -> some View {
        let present = usage.filter { $0.files > 0 }
        let missing = usage.flatMap(\.missing)
        let bytes = present.reduce(0) { $0 + $1.bytes }
        let files = present.reduce(0) { $0 + $1.files }
        return Form {
            if !present.isEmpty {
                Section {
                    StorageBar(usage: present)
                        .frame(height: 22)
                    let count = tr("storage.files \(Int(files))")
                    Text(tr("storage.total \(Format.bytes(bytes)) \(count)"))
                        .foregroundStyle(.secondary)
                }
                Section(tr("sidebar.categories")) {
                    ForEach(present, id: \.category) { u in
                        Button {
                            sidebar = .category(u.category)
                        } label: {
                            CategoryRow(usage: u)
                        }
                        .buttonStyle(.plain)
                        .help(tr("storage.showCategory \(u.category.title)"))
                    }
                }
            }
            if !missing.isEmpty {
                Section {
                    ForEach(missing, id: \.self) { id in
                        MissingRow(id: id)
                    }
                } header: {
                    HStack {
                        Text(tr("storage.missing"))
                        Spacer()
                        if missing.count > 1 {
                            Button(tr("storage.forgetAll")) {
                                model.remove(Set(missing), deleteFiles: false)
                            }
                            .controlSize(.small)
                        }
                    }
                } footer: {
                    Text(tr("storage.missingNote"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
    }
}

/// One segment per category, in category order, sized by bytes. The rows
/// below name each; the bar is never the only label.
private struct StorageBar: View {
    var usage: [CategoryUsage]

    var body: some View {
        let total = max(1, usage.reduce(0) { $0 + $1.bytes })
        GeometryReader { geo in
            let gaps = CGFloat(max(0, usage.count - 1))
            HStack(spacing: 1) {
                ForEach(usage, id: \.category) { u in
                    Rectangle()
                        .fill(u.category.color)
                        .frame(
                            width: max(
                                2, (geo.size.width - gaps) * CGFloat(u.bytes) / CGFloat(total))
                        )
                        .help(tr("storage.segment \(u.category.title) \(Format.bytes(u.bytes))"))
                }
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: 4))
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(tr("storage.barLabel"))
    }
}

private struct CategoryRow: View {
    var usage: CategoryUsage

    var body: some View {
        HStack {
            RoundedRectangle(cornerRadius: 3)
                .fill(usage.category.color)
                .frame(width: 12, height: 12)
            Label(usage.category.title, systemImage: usage.category.icon)
            Spacer()
            Text(tr("storage.files \(Int(usage.files))"))
                .foregroundStyle(.secondary)
            Text(Format.bytes(usage.bytes))
                .monospacedDigit()
                .frame(minWidth: 70, alignment: .trailing)
        }
        .contentShape(Rectangle())
    }
}

/// A completed download whose file is gone from where Dhow put it.
private struct MissingRow: View {
    @Environment(AppModel.self) private var model
    var id: String

    var body: some View {
        let d = model.store.downloads[id]
        HStack {
            Image(systemName: "questionmark.folder")
                .foregroundStyle(.tertiary)
            VStack(alignment: .leading) {
                Text(d?.fileName ?? "")
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(((d?.finalPath ?? "") as NSString).abbreviatingWithTildeInPath)
                    .font(.caption)
                    .lineLimit(1)
                    .truncationMode(.head)
            }
            .foregroundStyle(.secondary)
            Spacer()
            Button(tr("storage.forget")) { model.remove([id], deleteFiles: false) }
                .controlSize(.small)
        }
    }
}

extension DhowCore.Category {
    /// UI-GUIDELINES §6's palette: one decision shared by every shell.
    var color: Color {
        let (light, dark): (UInt32, UInt32) =
            switch self {
            case .video: (0x5B7CFA, 0x7E97FF)
            case .audio: (0xC85FD6, 0xDA82E6)
            case .image: (0x2CB67D, 0x4FD39A)
            case .document: (0xF2A33A, 0xFFB95C)
            case .archive: (0x8A6D3B, 0xB39160)
            case .software: (0xE25555, 0xF27777)
            case .code: (0x3BA7C4, 0x5EC4DD)
            case .data: (0x7F8C8D, 0xA0ACAD)
            case .other: (0xB0B7BC, 0xCAD0D4)
            }
        return Color(
            nsColor: NSColor(name: nil) { appearance in
                let isDark = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                return NSColor(rgb: isDark ? dark : light)
            })
    }
}

extension NSColor {
    convenience init(rgb: UInt32) {
        self.init(
            srgbRed: CGFloat((rgb >> 16) & 0xFF) / 255, green: CGFloat((rgb >> 8) & 0xFF) / 255,
            blue: CGFloat(rgb & 0xFF) / 255, alpha: 1)
    }
}
