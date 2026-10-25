import DhowCore
import SwiftUI

/// The one main window (UI-GUIDELINES §2, §3): sidebar, list, toolbar.
struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow
    @State private var search = ""
    @State private var selection = Set<ListRow.ID>()
    @AppStorage(Prefs.inspectorShown) private var inspectorShown = true

    var body: some View {
        @Bindable var model = model
        let rows =
            model.store.loaded && model.sidebar != .storage
            ? model.store.rows(for: model.sidebar ?? .filter(.all), search: search, now: .now)
            : []
        NavigationSplitView {
            Sidebar(selection: $model.sidebar)
                .navigationSplitViewColumnWidth(min: 160, ideal: 200)
        } detail: {
            content(rows)
        }
        .navigationTitle(tr("app.name"))
        .toolbar { MainToolbar() }
        .searchable(text: $search, placement: .toolbar, prompt: tr("search.prompt"))
        .sheet(item: $model.add) { prefill in
            AddSheet(prefill: prefill, queue: model.queueForAdd)
        }
        .alert(
            tr("alert.title"), isPresented: alertShown, presenting: model.alert,
            actions: { _ in Button(tr("button.ok")) {} },
            message: { Text($0.message) }
        )
        .modifier(ConfirmationDialogs(model: model))
        // The Download menu acts on what is selected here.
        .focusedSceneValue(\.selectedDownloads, rows.downloadIds(selection))
        .onPasteCommand(of: [.url, .plainText]) { _ in model.paste() }
        // Links dropped on the window open Add (UI-GUIDELINES §2).
        .dropDestination(for: URL.self) { urls, _ in
            let links = urls.map(\.absoluteString).filter { !AddModel.links(in: $0).isEmpty }
            model.add(links: links)
            return !links.isEmpty
        }
        .onAppear {
            let openWindow = openWindow
            model.openMainWindow = { openWindow(id: "main") }
        }
        .frame(minWidth: 640, minHeight: 360)
    }

    @ViewBuilder private func content(_ rows: [ListRow]) -> some View {
        @Bindable var model = model
        if let failure = model.openFailure {
            Message(text: failure)
        } else if !model.store.loaded {
            ProgressView().controlSize(.small)
        } else if model.sidebar == .storage {
            StorageView(sidebar: $model.sidebar)
        } else {
            Group {
                if rows.isEmpty {
                    EmptyList(isEmpty: model.store.downloads.isEmpty, searching: !search.isEmpty)
                } else {
                    DownloadTable(rows: rows, selection: $selection)
                }
            }
            // Selection fills the detail pane (UI-GUIDELINES §5).
            .inspector(isPresented: $inspectorShown) {
                DetailPane(ids: rows.downloadIds(selection))
                    .inspectorColumnWidth(min: 260, ideal: 320, max: 480)
            }
            .toolbar {
                ToolbarItem {
                    // ⌥⌘I is the View menu's.
                    Button {
                        inspectorShown.toggle()
                    } label: {
                        Label(tr("toolbar.inspector"), systemImage: "sidebar.trailing")
                    }
                    .help(tr("toolbar.inspector"))
                }
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
