import DhowCore
import SwiftUI

/// The Add dialog (UI-GUIDELINES §4): URL, filename, destination, queue,
/// and under "More" connections, speed limit, start mode and headers. One
/// link shows its probe in the form; several show as a compact list.
struct AddSheet: View {
    @Environment(AppModel.self) private var app
    @Environment(\.dismiss) private var dismiss
    @State private var add: AddModel
    @State private var more = false
    @State private var choosingFolder = false
    @State private var limitText = ""

    init(prefill: AddPrefill, queue: String) {
        _add = State(
            initialValue: AddModel(text: prefill.text, directory: Self.defaultFolder, queue: queue))
    }

    /// The shell's default folder setting (Settings, General), else
    /// Downloads.
    static let defaultFolderKey = Prefs.defaultFolder

    static var defaultFolder: URL {
        if let path = UserDefaults.standard.string(forKey: defaultFolderKey), !path.isEmpty {
            return URL(fileURLWithPath: path, isDirectory: true)
        }
        return FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask)[0]
    }

    var body: some View {
        @Bindable var add = add
        VStack(alignment: .leading, spacing: 0) {
            Form {
                TextField(tr("add.url"), text: $add.text, axis: .vertical)
                    .lineLimit(1...4)
                if add.items.count == 1, let item = add.items.first {
                    single(item)
                } else if add.items.count > 1 {
                    list
                }
                LabeledContent(tr("add.destination")) {
                    HStack {
                        Text(add.directory.path(percentEncoded: false))
                            .lineLimit(1)
                            .truncationMode(.head)
                            .help(add.directory.path(percentEncoded: false))
                        Button(tr("add.choose")) { choosingFolder = true }
                    }
                }
                Picker(tr("add.queue"), selection: $add.queue) {
                    ForEach(app.store.queues, id: \.id) { q in
                        Text(q.title).tag(q.id)
                    }
                }
                DisclosureGroup(tr("add.more"), isExpanded: $more) {
                    options
                }
            }
            .formStyle(.grouped)
            .fixedSize(horizontal: false, vertical: true)
            if let error = add.error {
                Text(error)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 20)
            }
            HStack {
                Spacer()
                Button(tr("button.cancel"), role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(add.isBlind ? tr("add.downloadAnyway") : tr("add.download")) {
                    confirm()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(!add.canConfirm)
            }
            .padding(20)
        }
        .frame(width: 520)
        .fileImporter(isPresented: $choosingFolder, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result {
                add.directory = url
            }
        }
        // Settings (General): the folder is asked for as the dialog opens.
        .onAppear {
            if UserDefaults.standard.bool(forKey: Prefs.askWhereToSave) {
                choosingFolder = true
            }
        }
        .task(id: add.probeKey) {
            // Typing settles before the core is asked.
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled, let engine = app.engine else { return }
            await add.probe(with: engine)
        }
        .onChange(of: add.headers) { add.reprobe() }
        .onChange(of: limitText) { add.speedLimitKB = Int(limitText).flatMap { $0 > 0 ? $0 : nil } }
    }

    @ViewBuilder private func single(_ item: AddModel.Item) -> some View {
        TextField(
            tr("add.filename"),
            text: Binding(get: { item.name }, set: { add.rename(item.id, to: $0) })
        )
        .overlay(alignment: .trailing) {
            if item.isProbing {
                ProgressView().controlSize(.small).padding(.trailing, 4)
            }
        }
        ProbeSummary(item: item)
    }

    private var list: some View {
        LabeledContent(tr("add.files")) {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(add.items) { item in
                    HStack {
                        if item.isProbing {
                            ProgressView().controlSize(.mini)
                            Text(item.url).lineLimit(1).truncationMode(.middle)
                                .foregroundStyle(.secondary)
                        } else {
                            Text(item.name.isEmpty ? item.url : item.name)
                                .lineLimit(1).truncationMode(.middle)
                            Spacer()
                            ProbeSummary(item: item, compact: true)
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder private var options: some View {
        let settings = app.store.settings
        let max = Int(settings?.maxConnectionsPerDownload ?? 16)
        Stepper(
            value: Binding(
                get: { add.connections ?? Int(settings?.defaultConnections ?? 8) },
                set: { add.connections = $0 }), in: 1...Swift.max(1, max)
        ) {
            Text(tr("add.connections \(add.connections ?? Int(settings?.defaultConnections ?? 8))"))
        }
        TextField(tr("add.speedLimit"), text: $limitText, prompt: Text(tr("add.noLimit")))
        Picker(tr("add.start"), selection: $add.start) {
            Text(tr("add.startNow")).tag(AddModel.Start.now)
            Text(tr("add.startQueued")).tag(AddModel.Start.queued)
            Text(tr("add.startPaused")).tag(AddModel.Start.paused)
        }
        .pickerStyle(.segmented)
        VStack(alignment: .leading) {
            Text(tr("add.headers"))
            TextEditor(text: $add.headers)
                .font(.body.monospaced())
                .frame(height: 54)
            if add.headerList == nil {
                Text(tr("add.headersInvalid")).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func confirm() {
        guard let engine = app.engine else { return }
        Task {
            if await add.confirm(with: engine) {
                dismiss()
            }
        }
    }
}

/// Size and resumability, or why the probe failed: calm, not red (§2, §4).
private struct ProbeSummary: View {
    var item: AddModel.Item
    var compact = false

    var body: some View {
        switch item.probe {
        case .running:
            EmptyView()
        case .done(let result):
            if compact {
                HStack(spacing: 4) {
                    if result.resumable == .no {
                        Image(systemName: "info.circle").help(tr("add.notResumable"))
                    }
                    Text(result.totalSize.map(Format.bytes) ?? tr("size.unknown"))
                }
                .foregroundStyle(.secondary)
            } else {
                LabeledContent(tr("add.size")) {
                    Text(result.totalSize.map(Format.bytes) ?? tr("size.unknown"))
                }
                if result.resumable == .no {
                    Label(tr("add.notResumable"), systemImage: "info.circle")
                        .foregroundStyle(.secondary)
                }
            }
        case .failed(let error):
            if compact {
                Image(systemName: "exclamationmark.circle")
                    .foregroundStyle(.secondary)
                    .help(ErrorText.message(error))
            } else {
                Label(ErrorText.message(error), systemImage: "exclamationmark.circle")
                    .foregroundStyle(.secondary)
                if ErrorText.blindCanHelp(error) {
                    Text(tr("add.blindNote")).font(.caption).foregroundStyle(.secondary)
                }
            }
        }
    }
}
