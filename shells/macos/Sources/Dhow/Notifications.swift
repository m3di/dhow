import AppKit
import DhowCore
@preconcurrency import UserNotifications

/// One user notification (UI-GUIDELINES §2 "Quiet"): a download completed
/// or failed, a group or a queue finished.
struct Note: Equatable {
    enum Kind: String { case completed, failed, group, queue }

    var kind: Kind
    var title: String
    var body: String
    /// The file it's about, for "Show in Finder" and a click.
    var downloadId: String?
    var sound: Bool
}

/// Posts notes. The app's posts through UserNotifications; tests record.
@MainActor
protocol Notifier: AnyObject {
    func post(_ note: Note)
}

/// Which notes a change makes, given the Notifications settings: one per
/// completed download unless it's in a group, which notifies once when all
/// its members are done; one per failure; one for a queue that ran dry
/// after finishing several (a lone download already said so).
enum NotePolicy {
    struct Settings {
        var completed = true
        var failed = true
        var groups = true
        var queues = true
        var sound = true

        static func read(_ d: UserDefaults = .standard) -> Settings {
            Settings(
                completed: d.bool(forKey: Prefs.notifyCompleted),
                failed: d.bool(forKey: Prefs.notifyFailed),
                groups: d.bool(forKey: Prefs.notifyGroups),
                queues: d.bool(forKey: Prefs.notifyQueues),
                sound: d.bool(forKey: Prefs.notifySound))
        }
    }

    @MainActor
    static func notes(for change: StoreChange, in store: DownloadStore, _ s: Settings) -> [Note] {
        switch change {
        case .finished(let d) where d.state == .failed:
            guard s.failed else { return [] }
            return [
                Note(
                    kind: .failed, title: tr("note.failed"),
                    body: tr("note.failedBody \(d.fileName) \(RowStatus.failure(d.failReason))"),
                    downloadId: d.id, sound: s.sound)
            ]
        case .finished(let d):
            let members = d.group.map { g in
                store.downloads.values.filter { $0.group == g && $0.state != .cancelled }
            }
            if let members, members.count > 1 {
                guard s.groups, members.allSatisfy({ $0.state == .completed }) else { return [] }
                let name = d.group.flatMap { store.groups[$0]?.name } ?? d.fileName
                return [
                    Note(
                        kind: .group, title: tr("note.groupCompleted"),
                        body: tr("note.groupBody \(name) \(members.count)"), downloadId: d.id,
                        sound: s.sound)
                ]
            }
            guard s.completed else { return [] }
            return [
                Note(
                    kind: .completed, title: tr("note.completed"), body: d.fileName,
                    downloadId: d.id, sound: s.sound)
            ]
        case .queueRanDry(let q, let finished):
            guard s.queues, finished > 1 else { return [] }
            return [
                Note(
                    kind: .queue, title: tr("note.queueFinished \(q.title)"),
                    body: tr("note.queueBody \(finished)"), downloadId: nil, sound: s.sound)
            ]
        }
    }
}

/// UserNotifications, asked for permission the first time there is
/// something to say (never at launch). Only inside Dhow.app: the framework
/// needs a bundle.
@MainActor
final class UserNotifier: NSObject, Notifier, UNUserNotificationCenterDelegate {
    /// A note was clicked (`false`) or its "Show in Finder" chosen (`true`).
    var onOpen: ((_ downloadId: String, _ reveal: Bool) -> Void)?
    private var authorized: Bool?
    private var waiting: [Note] = []

    nonisolated static let category = "download"
    nonisolated static let showInFinder = "showInFinder"

    /// `nil` outside an app bundle (`swift run`, tests).
    static func make() -> UserNotifier? {
        Bundle.main.bundleIdentifier == nil ? nil : UserNotifier()
    }

    func post(_ note: Note) {
        switch authorized {
        case true?: add(note)
        case false?: return
        case nil:
            waiting.append(note)
            if waiting.count == 1 {
                Task { await authorize() }
            }
        }
    }

    private func authorize() async {
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        let show = UNNotificationAction(
            identifier: Self.showInFinder, title: tr("menu.showInFinder"), options: [.foreground])
        center.setNotificationCategories([
            UNNotificationCategory(
                identifier: Self.category, actions: [show], intentIdentifiers: [])
        ])
        let granted = (try? await center.requestAuthorization(options: [.alert, .sound])) ?? false
        authorized = granted
        let notes = waiting
        waiting = []
        if granted {
            notes.forEach(add)
        }
    }

    private func add(_ note: Note) {
        let content = UNMutableNotificationContent()
        content.title = note.title
        content.body = note.body
        if note.sound {
            content.sound = .default
        }
        if let id = note.downloadId {
            content.userInfo = ["download": id]
            if note.kind != .failed {
                content.categoryIdentifier = Self.category
            }
        }
        let request = UNNotificationRequest(
            identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
    ) async {
        let id = response.notification.request.content.userInfo["download"] as? String
        let reveal = response.actionIdentifier == Self.showInFinder
        guard let id else { return }
        await MainActor.run { onOpen?(id, reveal) }
    }

    /// Shown while Dhow is in front too, as a banner.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }
}
