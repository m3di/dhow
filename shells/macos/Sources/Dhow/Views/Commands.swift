import AppKit
import DhowCore
import SwiftUI

/// The downloads the main window's selection stands for. (A key, not
/// `@Entry`: SwiftUI's macros need Xcode.)
struct SelectedDownloadsKey: FocusedValueKey {
    typealias Value = Set<String>
}

extension FocusedValues {
    var selectedDownloads: Set<String>? {
        get { self[SelectedDownloadsKey.self] }
        set { self[SelectedDownloadsKey.self] = newValue }
    }
}

/// The menu bar (UI-GUIDELINES §8 macOS): File > New Download, a Download
/// menu mirroring the list's context menu, View items for the inspector,
/// the sidebar's filters and Storage, and Help. Shortcuts follow
/// Transmission's where macOS has no convention (DECISIONS).
struct DhowCommands: Commands {
    var model: AppModel
    @FocusedValue(\.selectedDownloads) private var selected
    @AppStorage(Prefs.inspectorShown) private var inspectorShown = true

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button(tr("menu.newDownload")) { model.showAdd() }
                .keyboardShortcut("n")
                .disabled(model.engine == nil)
        }
        CommandMenu(tr("menu.download")) {
            // Nothing while a sheet or dialog is up: ⌘. there means Cancel.
            DownloadMenu(model: model, ids: model.isModal ? [] : selected ?? [])
                .disabled(model.isModal)
        }
        CommandGroup(before: .sidebar) {
            Button(inspectorShown ? tr("menu.hideInspector") : tr("menu.showInspector")) {
                inspectorShown.toggle()
            }
            .keyboardShortcut("i", modifiers: [.command, .option])
            Divider()
            ForEach(Array(SmartFilter.allCases.enumerated()), id: \.element) { i, f in
                Button(f.title) { model.sidebar = .filter(f) }
                    .keyboardShortcut(KeyEquivalent(Character(String(i + 1))))
            }
            Button(tr("sidebar.storage")) { model.sidebar = .storage }
                .keyboardShortcut("6")
            Divider()
        }
        CommandGroup(replacing: .help) {
            Button(tr("menu.website")) { NSWorkspace.shared.open(AboutTab.repository) }
        }
    }
}

/// The Download menu: what the context menu offers for the selection,
/// plus Pause All and Resume All.
private struct DownloadMenu: View {
    var model: AppModel
    var ids: Set<String>

    var body: some View {
        let selected = ids.compactMap { model.store.downloads[$0] }
        let states = selected.map(\.state)
        Button(tr("menu.pause")) { model.pause(ids) }
            .keyboardShortcut(".")
            .disabled(!states.contains(where: Actions.canPause))
        Button(RowMenu.resumeTitle(states)) { model.resume(ids) }
            .keyboardShortcut("/")
            .disabled(!states.contains(where: Actions.canResume))
        Button(tr("detail.retry")) {
            for d in selected where d.state == .failed {
                model.retry(d.id)
            }
        }
        .keyboardShortcut("/", modifiers: [.command, .control])
        .disabled(!states.contains(.failed))
        Divider()
        Button(tr("menu.open")) { model.open(ids) }
            .keyboardShortcut("o")
            .disabled(!states.contains(.completed))
        Button(tr("menu.showInFinder")) { model.showInFinder(ids) }
            .keyboardShortcut("r")
            .disabled(selected.isEmpty)
        Button(selected.count == 1 ? tr("menu.copyLink") : tr("menu.copyLinks")) {
            model.copyLinks(ids)
        }
        .keyboardShortcut("c", modifiers: [.command, .shift])
        .disabled(selected.isEmpty)
        Divider()
        Button(tr("menu.cancelEllipsis")) { model.confirmation = .cancel(ids) }
            .keyboardShortcut(".", modifiers: [.command, .shift])
            .disabled(!states.contains(where: Actions.canCancel))
        Button(tr("menu.remove")) { model.remove(ids, deleteFiles: false) }
            .disabled(selected.isEmpty)
        Button(tr("menu.removeAndDeleteEllipsis")) { model.confirmation = .delete(ids) }
            .keyboardShortcut(.delete, modifiers: [.command, .option])
            .disabled(selected.isEmpty)
        Divider()
        Button(tr("toolbar.pauseAll")) { Task { await model.pauseAll() } }
            .keyboardShortcut(".", modifiers: [.command, .option])
            .disabled(!model.canPauseAll)
        Button(tr("toolbar.resumeAll")) { Task { await model.resumeAll() } }
            .keyboardShortcut("/", modifiers: [.command, .option])
            .disabled(!model.canResumeAll)
    }
}
