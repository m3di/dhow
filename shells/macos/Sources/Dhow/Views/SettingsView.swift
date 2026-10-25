import AppKit
import DhowCore
import SwiftUI

/// The Settings window (UI-GUIDELINES §7, §8 macOS): toolbar tabs in §7's
/// order. The core's settings are one draft saved with one `set_settings`
/// patch, which the core validates; its refusal shows at the field it
/// names. The draft is the app's, so closing the window keeps unsaved
/// edits. The shell's own settings and queues apply as they change.
struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @State private var tab = SettingsTab.general

    private var form: SettingsForm { model.settingsForm }

    var body: some View {
        Group {
            if model.store.settings != nil {
                TabView(selection: $tab) {
                    GeneralTab()
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
                    QueuesTab()
                        .tabItem { Label(tr("settings.queues"), systemImage: "tray.2") }
                        .tag(SettingsTab.queues)
                    CategoriesTab(form: form)
                        .tabItem { Label(tr("settings.categoriesTab"), systemImage: "folder") }
                        .tag(SettingsTab.categories)
                    NotificationsTab()
                        .tabItem { Label(tr("settings.notifications"), systemImage: "bell") }
                        .tag(SettingsTab.notifications)
                    AdvancedTab()
                        .tabItem {
                            Label(tr("settings.advanced"), systemImage: "gearshape.2")
                        }
                        .tag(SettingsTab.advanced)
                    AboutTab()
                        .tabItem { Label(tr("settings.about"), systemImage: "info.circle") }
                        .tag(SettingsTab.about)
                }
                .safeAreaInset(edge: .bottom) {
                    if tab.savesDraft {
                        footer
                    }
                }
            } else {
                Text(model.openFailure ?? tr("settings.unavailable"))
                    .foregroundStyle(.secondary)
                    .padding(40)
            }
        }
        .frame(width: 720, height: 600)
        .onChange(of: model.store.settings, initial: true) { _, settings in
            if let settings {
                form.load(settings)
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
                    if let errorTab = await form.save({ try await model.saveSettings($0) }) {
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
