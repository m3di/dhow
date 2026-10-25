import DhowCore
import SwiftUI

/// Queues (UI-GUIDELINES §7.5): the list, and the chosen queue's editor
/// with its schedule. Each queue is saved on its own (`update_queue`).
struct QueuesTab: View {
    @Environment(AppModel.self) private var model
    @State private var selected: String?
    @State private var draft: QueueDraft?
    @State private var error: String?
    @State private var confirmingDelete = false
    /// A queue chosen while the shown one has unsaved edits: asked first.
    @State private var switchingTo: String?

    private var queue: QueueSnapshot? { selected.flatMap { model.store.queue($0) } }

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                List(selection: selection) {
                    ForEach(model.store.queues, id: \.id) { q in
                        Text(q.title).tag(q.id)
                    }
                }
                .listStyle(.bordered)
                HStack(spacing: 0) {
                    Button {
                        create()
                    } label: {
                        Label(tr("queue.add"), systemImage: "plus")
                    }
                    Button {
                        confirmingDelete = true
                    } label: {
                        Label(tr("queue.delete"), systemImage: "minus")
                    }
                    .disabled(queue?.isDefault != false)
                    Spacer()
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .padding(6)
            }
            .frame(width: 190)
            .padding([.leading, .vertical], 20)
            if let queue, let binding = Binding($draft) {
                VStack(spacing: 0) {
                    QueueEditor(draft: binding)
                    HStack {
                        if let error {
                            ErrorLine(message: error)
                        }
                        Spacer()
                        Button(tr("settings.revert")) { reset(queue) }
                            .disabled(!binding.wrappedValue.differs(from: queue))
                        Button(tr("settings.save")) { save(queue.id) }
                            .keyboardShortcut(.defaultAction)
                            .disabled(!binding.wrappedValue.differs(from: queue))
                    }
                    .padding(.horizontal, 20)
                    .padding(.bottom, 16)
                }
            } else {
                Text(tr("queue.none"))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onAppear {
            if selected == nil {
                selected = model.store.defaultQueue?.id
            }
        }
        .onChange(of: selected, initial: true) {
            if let queue { reset(queue) } else { draft = nil }
        }
        // Another change to the queue (its counts, the CLI) comes in
        // unless the user is editing it.
        .onChange(of: queue) { old, new in
            guard let new else { return }
            if draft == nil || old.map({ draft?.differs(from: $0) == false }) ?? true {
                reset(new)
            }
        }
        .confirmationDialog(
            Self.unsavedTitle(queue), isPresented: asking, presenting: switchingTo
        ) { next in
            Button(tr("settings.save")) {
                if let id = selected { save(id, then: next) }
            }
            Button(tr("queue.discard"), role: .destructive) { selected = next }
            Button(tr("button.cancel"), role: .cancel) {}
        }
        .confirmationDialog(
            Self.deleteTitle(queue), isPresented: $confirmingDelete
        ) {
            Button(tr("queue.delete"), role: .destructive) { delete() }
        } message: {
            Text(tr("queue.deleteMessage"))
        }
    }

    /// The list's selection; leaving a queue with unsaved edits asks.
    private var selection: Binding<String?> {
        Binding(
            get: { selected },
            set: { next in
                if let queue, let draft, draft.differs(from: queue), next != selected {
                    switchingTo = next
                } else {
                    selected = next
                }
            })
    }

    private var asking: Binding<Bool> {
        Binding(get: { switchingTo != nil }, set: { if !$0 { switchingTo = nil } })
    }

    static func unsavedTitle(_ q: QueueSnapshot?) -> String {
        let name = q?.title ?? ""
        return tr("queue.unsavedTitle \(name)")
    }

    static func deleteTitle(_ q: QueueSnapshot?) -> String {
        let name = q?.title ?? ""
        return tr("queue.deleteTitle \(name)")
    }

    private func reset(_ q: QueueSnapshot) {
        draft = QueueDraft(q)
        error = nil
    }

    /// Saves the queue's edits, then (once they're stored) shows `next`.
    private func save(_ id: String, then next: String? = nil) {
        guard let draft else { return }
        Task {
            do {
                let spec = try draft.spec()
                _ = try await model.queueCommand { try $0.updateQueue(id: id, spec: spec) }
                error = nil
                if let next {
                    selected = next
                }
            } catch {
                self.error = QueueText.invalid(error)
            }
        }
    }

    private func create() {
        let spec = QueueSpec(name: tr("queue.newName"), onComplete: .nothing)
        Task {
            do {
                if let q = try await model.queueCommand({ try $0.createQueue(spec: spec) }) {
                    selected = q.id
                }
            } catch {
                self.error = QueueText.invalid(error)
            }
        }
    }

    private func delete() {
        guard let id = selected, let fallback = model.store.defaultQueue?.id else { return }
        Task {
            do {
                _ = try await model.queueCommand {
                    try $0.deleteQueue(id: id, moveItemsTo: fallback)
                }
                selected = fallback
            } catch {
                self.error = QueueText.invalid(error)
            }
        }
    }
}

enum QueueText {
    static func invalid(_ error: Error) -> String {
        switch error {
        case is SettingsDraft.OutOfRange, DhowError.InvalidQueue: tr("queue.invalid")
        default: ErrorText.message(error)
        }
    }

    static func onComplete(_ o: OnComplete) -> String {
        switch o {
        case .nothing: tr("queue.whenDoneNothing")
        case .sleep: tr("queue.whenDoneSleep")
        case .shutdown: tr("queue.whenDoneShutDown")
        case .hibernate: tr("queue.whenDoneHibernate")
        }
    }
}

private struct QueueEditor: View {
    @Environment(AppModel.self) private var model
    @Binding var draft: QueueDraft
    /// macOS won't let Dhow send the shut-down request (Automation).
    @State private var shutDownBlocked = false

    var body: some View {
        Form {
            Section {
                TextField(
                    tr("queue.name"), text: $draft.name,
                    prompt: draft.isDefault ? Text(tr("queue.default")) : nil)
                NumberRow(title: tr("queue.maxConcurrent"), value: $draft.maxConcurrent)
                Toggle(tr("queue.limit"), isOn: $draft.limitOn)
                if draft.limitOn {
                    NumberRow(
                        title: tr("settings.limit"), value: $draft.limitKB,
                        unit: tr("settings.unit.kbps"))
                }
                Picker(tr("queue.whenDone"), selection: whenDone) {
                    ForEach([OnComplete.nothing, .sleep, .shutdown], id: \.self) {
                        Text(QueueText.onComplete($0)).tag($0)
                    }
                    // Set elsewhere (the CLI); on a Mac it sleeps.
                    if draft.onComplete == .hibernate {
                        Text(QueueText.onComplete(.hibernate)).tag(OnComplete.hibernate)
                    }
                }
            } footer: {
                if draft.onComplete != .nothing {
                    Text(tr("queue.whenDoneNote"))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if draft.onComplete == .shutdown, shutDownBlocked {
                    ErrorLine(message: tr("queue.shutDownBlocked"))
                }
            }
            if !draft.isDefault {
                Section {
                    Toggle(tr("queue.scheduled"), isOn: $draft.scheduled)
                    if draft.scheduled {
                        Picker(tr("queue.scheduleAction"), selection: $draft.action) {
                            Text(tr("queue.actionStart")).tag(ScheduleAction.start)
                            Text(tr("queue.actionWithin")).tag(ScheduleAction.startAndStopOutside)
                        }
                        ForEach($draft.windows) { $window in
                            WindowRow(window: $window) {
                                draft.windows.removeAll { $0.id == window.id }
                            }
                        }
                        Button(tr("queue.addWindow")) {
                            draft.windows.append(QueueDraft.newWindow())
                        }
                    }
                } header: {
                    Text(tr("queue.schedule"))
                }
            }
        }
        .formStyle(.grouped)
    }

    /// Shut Down chosen by the user: macOS asks now (if it asks at all),
    /// with them here, rather than at the end of the night.
    private var whenDone: Binding<OnComplete> {
        Binding(
            get: { draft.onComplete },
            set: { choice in
                draft.onComplete = choice
                if choice == .shutdown {
                    Task { shutDownBlocked = !(await model.mayShutDown()) }
                }
            })
    }
}

/// One time window: its days and hours.
private struct WindowRow: View {
    @Binding var window: QueueDraft.Window
    var remove: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 4) {
                let symbols = Calendar.current.shortWeekdaySymbols
                ForEach(QueueDraft.week, id: \.self) { day in
                    Toggle(
                        symbols[Self.index(day)],
                        isOn: Binding(
                            get: { window.days.contains(day) },
                            set: { on in
                                if on { window.days.insert(day) } else { window.days.remove(day) }
                            })
                    )
                    .toggleStyle(.button)
                    .controlSize(.small)
                }
                Spacer()
                Button(action: remove) {
                    Label(tr("queue.removeWindow"), systemImage: "minus.circle")
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
            }
            HStack {
                DatePicker(
                    tr("queue.from"),
                    selection: Binding(
                        get: { window.start.date }, set: { window.start = TimeOfDay($0) }),
                    displayedComponents: .hourAndMinute)
                DatePicker(
                    tr("queue.to"),
                    selection: Binding(
                        get: { window.end.date }, set: { window.end = TimeOfDay($0) }),
                    displayedComponents: .hourAndMinute)
            }
        }
    }

    /// `shortWeekdaySymbols` starts on Sunday.
    static func index(_ day: Weekday) -> Int {
        switch day {
        case .sunday: 0
        case .monday: 1
        case .tuesday: 2
        case .wednesday: 3
        case .thursday: 4
        case .friday: 5
        case .saturday: 6
        }
    }
}
