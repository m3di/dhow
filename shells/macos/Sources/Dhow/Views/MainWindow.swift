import DhowCore
import SwiftUI

/// The one main window (UI-GUIDELINES §2, §3): sidebar, list, toolbar.
struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @State private var sidebar: SidebarItem? = .filter(.all)
    @State private var search = ""
    @State private var selection = Set<ListRow.ID>()

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            Sidebar(selection: $sidebar)
                .navigationSplitViewColumnWidth(min: 160, ideal: 200)
        } detail: {
            content
        }
        .navigationTitle(tr("app.name"))
        .toolbar { MainToolbar() }
        .searchable(text: $search, placement: .toolbar, prompt: tr("search.prompt"))
        .sheet(item: $model.add) { prefill in
            AddSheet(prefill: prefill, queue: model.store.defaultQueue?.id ?? "")
        }
        .alert(
            tr("alert.title"), isPresented: alertShown, presenting: model.alert,
            actions: { _ in Button(tr("button.ok")) {} },
            message: { Text($0.message) }
        )
        .onPasteCommand(of: [.url, .plainText]) { _ in model.paste() }
        .frame(minWidth: 640, minHeight: 360)
    }

    @ViewBuilder private var content: some View {
        if let failure = model.openFailure {
            Message(text: failure)
        } else if !model.store.loaded {
            ProgressView().controlSize(.small)
        } else {
            let rows = model.store.rows(
                for: sidebar ?? .filter(.all), search: search, now: .now)
            if rows.isEmpty {
                EmptyList(isEmpty: model.store.downloads.isEmpty, searching: !search.isEmpty)
            } else {
                DownloadTable(rows: rows, selection: $selection)
            }
        }
    }

    private var alertShown: Binding<Bool> {
        Binding(get: { model.alert != nil }, set: { if !$0 { model.alert = nil } })
    }
}

/// One line and the Add button (§3); no illustration bigger than an icon.
private struct EmptyList: View {
    @Environment(AppModel.self) private var model
    var isEmpty: Bool
    var searching: Bool

    var body: some View {
        VStack(spacing: 12) {
            Text(
                searching
                    ? tr("empty.search") : isEmpty ? tr("empty.none") : tr("empty.filter")
            )
            .foregroundStyle(.secondary)
            if isEmpty {
                Button(tr("button.addDownload")) { model.showAdd() }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct Message: View {
    var text: String

    var body: some View {
        Text(text)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .padding()
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
