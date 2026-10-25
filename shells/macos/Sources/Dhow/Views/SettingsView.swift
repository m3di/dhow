import AppKit
import DhowCore
import SwiftUI

/// The Settings window (UI-GUIDELINES §7, §8 macOS): toolbar tabs over one
/// draft, saved with one `set_settings` patch. The core validates it and
/// its refusal shows at the field it names. The draft is the app's, so
/// closing the window keeps unsaved edits for the next time it opens.
struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var tab = SettingsTab.general

    private var form: SettingsForm { model.settingsForm }

    var body: some View {
        Group {
            if model.store.settings != nil {
                TabView(selection: $tab) {
                    GeneralTab(form: form)
                        .tabItem { Label(tr("settings.general"), systemImage: "gearshape") }
                        .tag(SettingsTab.general)
                    ConnectionsTab(form: form)
                        .tabItem {
                            Label(
                                tr("settings.connections"),
                                systemImage: "point.3.connected.trianglepath.dotted")
                        }
                        .tag(SettingsTab.connections)
                    SpeedTab(form: form)
                        .tabItem { Label(tr("settings.speed"), systemImage: "speedometer") }
                        .tag(SettingsTab.speed)
                    NetworkTab(form: form)
                        .tabItem { Label(tr("settings.network"), systemImage: "network") }
                        .tag(SettingsTab.network)
                    CategoriesTab(form: form)
                        .tabItem { Label(tr("settings.categories"), systemImage: "folder") }
                        .tag(SettingsTab.categories)
                }
                .safeAreaInset(edge: .bottom) { footer }
            } else {
                Text(model.openFailure ?? tr("settings.unavailable"))
                    .foregroundStyle(.secondary)
                    .padding(40)
            }
        }
        .frame(width: 540, height: 600)
        .onChange(of: model.store.settings, initial: true) { _, settings in
            if let settings {
                form.load(settings, folder: AddSheet.defaultFolder.path(percentEncoded: false))
            }
        }
    }

    private var footer: some View {
        HStack {
            // An error the core didn't pin on a field.
            if let failure = form.failure, failure.key.flatMap(SettingsTab.of) == nil {
                Text(failure.message)
                    .foregroundStyle(Color(nsColor: .systemRed))
                    .lineLimit(2)
            }
            Spacer()
            Button(tr("settings.revert")) { form.revert() }
                .disabled(!form.isDirty)
            Button(tr("settings.save")) {
                Task {
                    let errorTab = await form.save(
                        { try await model.saveSettings($0) },
                        folder: { UserDefaults.standard.set($0, forKey: AddSheet.defaultFolderKey) }
                    )
                    if let errorTab {
                        tab = errorTab
                    }
                }
            }
            .keyboardShortcut(.defaultAction)
            .disabled(!form.isDirty || form.saving)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .background(.bar)
    }
}

/// A field's refusal, under it.
private struct FieldError: View {
    var form: SettingsForm
    var key: String

    var body: some View {
        if let message = form.error(for: key) {
            ErrorLine(message: message)
        }
    }
}

private struct ErrorLine: View {
    var message: String

    var body: some View {
        Text(message)
            .font(.caption)
            .foregroundStyle(Color(nsColor: .systemRed))
    }
}

/// A whole number with a stepper beside it; the field takes any value,
/// which the core accepts or refuses on save.
private struct NumberRow: View {
    var title: String
    @Binding var value: Int
    var unit: String?

    var body: some View {
        LabeledContent(title) {
            HStack(spacing: 4) {
                TextField(title, value: $value, format: .number.grouping(.never))
                    .labelsHidden()
                    .multilineTextAlignment(.trailing)
                    .frame(width: 64)
                Stepper(title, value: $value).labelsHidden()
                if let unit {
                    Text(unit).foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// Seconds, to a tenth.
private struct SecondsRow: View {
    var title: String
    @Binding var value: Double

    var body: some View {
        LabeledContent(title) {
            HStack(spacing: 4) {
                TextField(
                    // To the millisecond the core stores, so nothing rounds.
                    title, value: $value, format: .number.precision(.fractionLength(0...3))
                )
                .labelsHidden()
                .multilineTextAlignment(.trailing)
                .frame(width: 64)
                Text(tr("settings.unit.seconds")).foregroundStyle(.secondary)
            }
        }
    }
}

private struct GeneralTab: View {
    @Bindable var form: SettingsForm
    @State private var choosing = false

    var body: some View {
        Form {
            LabeledContent(tr("settings.defaultFolder")) {
                HStack {
                    Text(
                        (form.draft.defaultFolder as NSString).abbreviatingWithTildeInPath
                    )
                    .lineLimit(1)
                    .truncationMode(.head)
                    .help(form.draft.defaultFolder)
                    Button(tr("add.choose")) { choosing = true }
                }
            }
        }
        .formStyle(.grouped)
        .fileImporter(isPresented: $choosing, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result {
                form.draft.defaultFolder = url.path(percentEncoded: false)
            }
        }
    }
}

private struct ConnectionsTab: View {
    @Bindable var form: SettingsForm
    @State private var advanced = false

    var body: some View {
        Form {
            Section {
                NumberRow(
                    title: tr("settings.defaultConnections"),
                    value: $form.draft.defaultConnections)
                FieldError(form: form, key: "default_connections")
                NumberRow(
                    title: tr("settings.maxConnections"),
                    value: $form.draft.maxConnectionsPerDownload)
                FieldError(form: form, key: "max_connections_per_download")
            }
            Section {
                NumberRow(
                    title: tr("settings.globalConnections"),
                    value: $form.draft.globalConnections)
                FieldError(form: form, key: "global_connections")
                NumberRow(
                    title: tr("settings.perHostConnections"),
                    value: $form.draft.perHostConnections)
                FieldError(form: form, key: "per_host_connections")
                Toggle(
                    tr("settings.newcomerConnection"), isOn: $form.draft.newcomerGetsConnection)
                FieldError(form: form, key: "min_connections_for_new")
            } footer: {
                // The engine reads the budget when it opens (ARCHITECTURE §3.1).
                Text(tr("settings.appliesAfterRestart"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                DisclosureGroup(tr("settings.advanced"), isExpanded: $advanced) {
                    SecondsRow(
                        title: tr("settings.firstByteTimeout"),
                        value: $form.draft.firstByteTimeout)
                    FieldError(form: form, key: "first_byte_timeout_ms")
                    SecondsRow(title: tr("settings.idleTimeout"), value: $form.draft.idleTimeout)
                    FieldError(form: form, key: "idle_timeout_ms")
                    NumberRow(title: tr("settings.maxRetries"), value: $form.draft.maxRetries)
                    FieldError(form: form, key: "max_retries")
                    Toggle(tr("settings.probeRangeGet"), isOn: $form.draft.probeRangeGet)
                }
            } footer: {
                Text(tr("settings.appliesNextRun"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onChange(of: form.failure) {
            if let key = form.failure?.key,
                ["first_byte_timeout_ms", "idle_timeout_ms", "max_retries"].contains(key)
            {
                advanced = true
            }
        }
    }
}

private struct SpeedTab: View {
    @Bindable var form: SettingsForm
    @State private var advanced = false

    var body: some View {
        Form {
            Section {
                Toggle(tr("settings.limitTotal"), isOn: $form.draft.limitOn)
                if form.draft.limitOn {
                    NumberRow(
                        title: tr("settings.limit"), value: $form.draft.limitKB,
                        unit: tr("settings.unit.kbps"))
                }
                FieldError(form: form, key: "global_speed_limit")
                NumberRow(
                    title: tr("settings.headroom"), value: $form.draft.headroomPercent,
                    unit: tr("settings.unit.percent"))
                FieldError(form: form, key: "headroom_percent")
            } footer: {
                Text(tr("settings.headroomNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                DisclosureGroup(tr("settings.advanced"), isExpanded: $advanced) {
                    SecondsRow(
                        title: tr("settings.commitInterval"), value: $form.draft.commitInterval)
                    FieldError(form: form, key: "commit_interval_ms")
                }
            } footer: {
                Text(tr("settings.commitNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onChange(of: form.failure) {
            if form.failure?.key == "commit_interval_ms" {
                advanced = true
            }
        }
    }
}

private struct NetworkTab: View {
    @Environment(AppModel.self) private var model
    @Bindable var form: SettingsForm

    var body: some View {
        Form {
            Section {
                Picker(tr("settings.proxy"), selection: $form.draft.proxy) {
                    Text(tr("settings.proxySystem")).tag(SettingsDraft.Proxy.system)
                    Text(tr("settings.proxyNone")).tag(SettingsDraft.Proxy.direct)
                    Text(tr("settings.proxyCustom")).tag(SettingsDraft.Proxy.custom)
                }
                if form.draft.proxy == .custom {
                    TextField(
                        tr("settings.proxyHttp"), text: $form.draft.proxyHttp,
                        prompt: Text(tr("settings.proxyPrompt")))
                    TextField(
                        tr("settings.proxyHttps"), text: $form.draft.proxyHttps,
                        prompt: Text(tr("settings.proxyPrompt")))
                    TextField(
                        tr("settings.proxyOther"), text: $form.draft.proxyOther,
                        prompt: Text(tr("settings.proxySocksPrompt")))
                    TextField(
                        tr("settings.noProxy"), text: $form.draft.noProxy,
                        prompt: Text(tr("settings.noProxyPrompt")))
                }
                FieldError(form: form, key: "proxy_mode")
            } footer: {
                Text(tr("settings.proxyNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section(tr("settings.currentNetwork")) {
                NetworkCard(info: model.store.network)
                HStack {
                    Spacer()
                    Button(tr("settings.redetect")) { model.readNetwork(announce: true) }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { model.readNetwork(announce: false) }
    }
}

/// What the adapter sees, read-only (§7.4).
private struct NetworkCard: View {
    var info: NetworkInfo?

    var body: some View {
        if let info {
            LabeledContent(tr("settings.interface"), value: Self.interface(info.interfaceKind))
            LabeledContent(
                tr("settings.vpn"), value: info.vpnActive ? tr("settings.yes") : tr("settings.no"))
            LabeledContent(
                tr("settings.systemProxy"),
                value: info.systemProxy == nil ? tr("settings.no") : tr("settings.yes"))
            LabeledContent(tr("settings.proxyInUse"), value: Self.mode(info.proxyInUse))
        } else {
            Text(tr("settings.networkUnknown")).foregroundStyle(.secondary)
        }
    }

    static func interface(_ kind: InterfaceKind) -> String {
        switch kind {
        case .wifi: tr("network.wifi")
        case .ethernet: tr("network.ethernet")
        case .cellular: tr("network.cellular")
        case .vpn: tr("network.vpn")
        case .unknown: tr("network.unknown")
        }
    }

    static func mode(_ mode: ProxyMode) -> String {
        switch mode {
        case .system: tr("settings.proxySystem")
        case .direct: tr("settings.proxyNone")
        case .custom: tr("settings.proxyCustom")
        }
    }
}

private struct CategoriesTab: View {
    @Bindable var form: SettingsForm

    var body: some View {
        Form {
            Section {
                Toggle(tr("settings.autoGrouping"), isOn: $form.draft.autoGrouping)
                Toggle(
                    tr("settings.useContentDisposition"), isOn: $form.draft.useContentDisposition)
                Toggle(tr("settings.appendTypeExtension"), isOn: $form.draft.appendTypeExtension)
            } footer: {
                Text(tr("settings.namingNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                ForEach(DhowCore.Category.allCases, id: \.self) { c in
                    TextField(
                        c.title,
                        text: Binding(
                            get: { form.draft.extraExtensions[c] ?? "" },
                            set: { form.draft.extraExtensions[c] = $0 }),
                        prompt: Text(tr("settings.extensionsPrompt")))
                    if let message = form.error(for: c) {
                        ErrorLine(message: message)
                    }
                }
            } header: {
                Text(tr("settings.extraExtensions"))
            } footer: {
                Text(tr("settings.extensionsNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }
}
