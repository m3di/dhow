import DhowCore
import Foundation

/// A queue as the Queues tab edits it (UI-GUIDELINES §7.5): its name,
/// concurrency, speed limit, what to do when it runs dry, and its schedule.
struct QueueDraft: Equatable {
    struct Window: Equatable, Identifiable {
        var id = UUID()
        var days: Set<Weekday>
        var start: TimeOfDay
        var end: TimeOfDay
    }

    var isDefault = false
    /// Empty on the default queue: the shell's name for it.
    var name = ""
    var maxConcurrent = 3
    var limitOn = false
    /// kB/s (1000 bytes), as everywhere.
    var limitKB = 1000
    var onComplete = OnComplete.nothing
    var scheduled = false
    var windows: [Window] = []
    var action = ScheduleAction.start

    static let week: [Weekday] = [
        .monday, .tuesday, .wednesday, .thursday, .friday, .saturday, .sunday,
    ]

    init() {}

    init(_ q: QueueSnapshot) {
        isDefault = q.isDefault
        name = q.name ?? ""
        maxConcurrent = Int(q.maxConcurrent)
        limitOn = q.speedLimit != nil
        limitKB = Int(clamping: (q.speedLimit ?? 1_000_000) / 1000)
        onComplete = q.onComplete
        scheduled = q.schedule != nil
        windows = (q.schedule?.windows ?? []).map {
            Window(days: Set($0.days), start: $0.start, end: $0.end)
        }
        action = q.schedule?.action ?? .start
    }

    /// A new window: every day, 01:00 to 07:00.
    static func newWindow() -> Window {
        Window(
            days: Set(week), start: TimeOfDay(hour: 1, minute: 0, second: 0),
            end: TimeOfDay(hour: 7, minute: 0, second: 0))
    }

    /// The spec `update_queue` takes; the core checks the rest (a name, at
    /// least one download at once, each window with a day).
    func spec() throws(SettingsDraft.OutOfRange) -> QueueSpec {
        guard let concurrent = UInt32(exactly: maxConcurrent) else {
            throw .init(key: "max_concurrent")
        }
        var limit: UInt64?
        if limitOn {
            guard let kb = UInt64(exactly: limitKB) else { throw .init(key: "speed_limit") }
            let (bytes, overflow) = kb.multipliedReportingOverflow(by: 1000)
            guard !overflow else { throw .init(key: "speed_limit") }
            limit = bytes
        }
        let schedule =
            scheduled && !isDefault
            ? Schedule(
                windows: windows.map {
                    ScheduleWindow(
                        days: Self.week.filter($0.days.contains), start: $0.start, end: $0.end)
                }, action: action) : nil
        return QueueSpec(
            name: name.trimmingCharacters(in: .whitespaces), maxConcurrent: concurrent,
            speedLimit: limit, schedule: schedule, onComplete: onComplete)
    }

    /// Edited: its spec differs from the queue's (window ids aside).
    func differs(from q: QueueSnapshot) -> Bool {
        guard let mine = try? spec() else { return true }
        return mine != (try? QueueDraft(q).spec())
    }
}

extension TimeOfDay {
    /// Today at this time, for a time picker.
    var date: Date {
        Calendar.current.date(
            bySettingHour: Int(hour), minute: Int(minute), second: 0, of: Date()) ?? Date()
    }

    init(_ date: Date) {
        let c = Calendar.current.dateComponents([.hour, .minute], from: date)
        self.init(hour: UInt8(c.hour ?? 0), minute: UInt8(c.minute ?? 0), second: 0)
    }
}
