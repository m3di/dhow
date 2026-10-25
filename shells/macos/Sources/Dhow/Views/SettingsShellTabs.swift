import AppKit
import DhowCore
import ServiceManagement
import SwiftUI
import UserNotifications

/// General (UI-GUIDELINES §7.1): the shell's settings, applied as they
/// change.
struct GeneralTab: View {
    @Environment(AppModel.self) private var model
    @AppStorage(Prefs.defaultFolder) private var folder = ""
    @AppStorage(Prefs.askWhereToSave) private var ask = false
    @AppStorage(Prefs.defaultQueue) private var queue = ""
    @AppStorage(Prefs.openFromNotification) private var openFromNote = true
    @State private var choosing = false
    @State private var login = LoginItem()

    var body: some View {
        Form {
            Section {
                LabeledContent(tr("settings.defaultFolder")) {
                    HStack {
                        let path = AddSheet.defaultFolder.path(percentEncoded: false)
                        Text((path as NSString).abbreviatingWithTildeInPath)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .help(path)
                        Button(tr("add.choose")) { choosing = true }
                    }
                }
                Toggle(tr("settings.askWhereToSave"), isOn: $ask)
                Picker(
                    tr("settings.defaultQueue"),
                    selection: Binding(get: { model.queueForAdd }, set: { queue = $0 })
                ) {
                    ForEach(model.store.queues, id: \.id) { q in
                        Text(q.title).tag(q.id)
                    }
                }
            }
            Section {
                Toggle(tr("settings.openFromNotification"), isOn: $openFromNote)
            }
            Section {
                Toggle(
                    tr("settings.launchAtLogin"),
                    isOn: Binding(get: { login.enabled }, set: { login.set($0) }))
                if login.needsApproval {
                    HStack {
                        Text(tr("settings.loginApproval"))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button(tr("settings.openLoginItems")) {
                            SMAppService.openSystemSettingsLoginItems()
                        }
                    }
                }
                if login.failed {
                    ErrorLine(message: tr("settings.loginFailed"))
                }
            }
        }
        .formStyle(.grouped)
        .fileImporter(isPresented: $choosing, allowedContentTypes: [.folder]) { result in
            if case .success(let url) = result {
                folder = url.path(percentEncoded: false)
            }
        }
        .onAppear { login.refresh() }
    }
}

/// Launch at login, as the system has it (`SMAppService`); it may ask the
/// user to allow it in System Settings.
@MainActor @Observable
final class LoginItem {
    private(set) var status = SMAppService.Status.notRegistered
    private(set) var failed = false

    var enabled: Bool { status == .enabled || status == .requiresApproval }
    var needsApproval: Bool { status == .requiresApproval }

    func refresh() {
        status = SMAppService.mainApp.status
    }

    func set(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            failed = false
        } catch {
            failed = true
        }
        refresh()
    }
}

/// Notifications (§7.7): which ones, and their sound. Dhow asks for
/// permission the first time it has something to say, not here.
struct NotificationsTab: View {
    @AppStorage(Prefs.notifyCompleted) private var completed = true
    @AppStorage(Prefs.notifyFailed) private var failed = true
    @AppStorage(Prefs.notifyGroups) private var groups = true
    @AppStorage(Prefs.notifyQueues) private var queues = true
    @AppStorage(Prefs.notifySound) private var sound = true
    @State private var denied = false

    var body: some View {
        Form {
            Section {
                Toggle(tr("settings.notifyCompleted"), isOn: $completed)
                Toggle(tr("settings.notifyFailed"), isOn: $failed)
                Toggle(tr("settings.notifyGroups"), isOn: $groups)
                Toggle(tr("settings.notifyQueues"), isOn: $queues)
            } footer: {
                Text(tr("settings.notifyNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                Toggle(tr("settings.notifySound"), isOn: $sound)
            }
            if denied {
                Section {
                    HStack {
                        Text(tr("settings.notificationsOff"))
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button(tr("settings.openNotifications")) {
                            if let url = URL(
                                string:
                                    "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
                            ) {
                                NSWorkspace.shared.open(url)
                            }
                        }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .task {
            // Reading the status never prompts; outside the app there's none.
            guard Bundle.main.bundleIdentifier != nil else { return }
            let status = await UNUserNotificationCenter.current().notificationSettings()
                .authorizationStatus
            denied = status == .denied
        }
    }
}

/// Advanced (§7.8): the log, and the segment map.
struct AdvancedTab: View {
    @AppStorage(Prefs.logLevel) private var level = LogLevel.standard.rawValue
    @AppStorage(Prefs.showSegmentMap) private var segmentMap = true

    var body: some View {
        Form {
            Section {
                Picker(tr("settings.logLevel"), selection: $level) {
                    Text(tr("settings.logErrors")).tag(LogLevel.errors.rawValue)
                    Text(tr("settings.logStandard")).tag(LogLevel.standard.rawValue)
                    Text(tr("settings.logDetailed")).tag(LogLevel.detailed.rawValue)
                }
                HStack {
                    Spacer()
                    Button(tr("settings.openLogs")) { openLogs() }
                        .disabled(AppModel.logDirectory == nil)
                }
            } footer: {
                Text(tr("settings.logNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                Toggle(tr("settings.showSegmentMap"), isOn: $segmentMap)
            } footer: {
                Text(tr("settings.segmentMapNote"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    private func openLogs() {
        guard let dir = AppModel.logDirectory else { return }
        let url = URL(fileURLWithPath: dir, isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        NSWorkspace.shared.open(url)
    }
}

/// About (§7.9): version, license, credits.
struct AboutTab: View {
    static let repository = URL(string: "https://github.com/m3di/dhow")!
    static let license = URL(string: "https://www.gnu.org/licenses/gpl-3.0.html")!
    static let uniffi = URL(string: "https://github.com/mozilla/uniffi-rs")!

    var body: some View {
        VStack(spacing: 14) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 96, height: 96)
                .accessibilityHidden(true)
            Text(tr("app.name")).font(.title)
            Text(tr("about.version \(Self.version) \(coreVersion())"))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
            Text(tr("about.license"))
                .multilineTextAlignment(.center)
            HStack(spacing: 16) {
                Link(tr("about.licenseLink"), destination: Self.license)
                Link(tr("about.source"), destination: Self.repository)
            }
            Divider().frame(width: 320)
            Text(tr("about.credits"))
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 460)
            Link(tr("about.uniffiSource"), destination: Self.uniffi)
                .font(.callout)
        }
        .padding(30)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? coreVersion()
    }
}
