import Foundation

/// Numbers as the list shows them, in the user's locale.
enum Format {
    static func bytes(_ n: UInt64) -> String {
        // "0 bytes", not "Zero KB".
        Int64(clamping: n).formatted(.byteCount(style: .file, spellsOutZero: false))
    }

    /// "4.1 MB/s".
    static func speed(_ bytesPerSec: UInt64) -> String {
        tr("format.speed \(bytes(bytesPerSec))")
    }

    /// "3 min", "1 hr": the largest unit only, as a row has room for.
    static func duration(seconds: UInt64) -> String {
        Duration.seconds(Int64(clamping: seconds)).formatted(
            .units(
                allowed: [.days, .hours, .minutes, .seconds], width: .abbreviated,
                maximumUnitCount: 1))
    }

    /// "15:48" today, "Oct 24" this year, else "Oct 24, 2025".
    static func added(unixMs: Int64, now: Date) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(unixMs) / 1000)
        let cal = Calendar.current
        if cal.isDate(date, inSameDayAs: now) {
            return date.formatted(date: .omitted, time: .shortened)
        }
        if cal.isDate(date, equalTo: now, toGranularity: .year) {
            return date.formatted(.dateTime.month(.abbreviated).day())
        }
        return date.formatted(.dateTime.month(.abbreviated).day().year())
    }

    /// "02:00" today, "Tue 02:00" within a week, else the date.
    static func time(unixMs: Int64, now: Date) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(unixMs) / 1000)
        let cal = Calendar.current
        if cal.isDate(date, inSameDayAs: now) {
            return date.formatted(date: .omitted, time: .shortened)
        }
        if date > now, date.timeIntervalSince(now) < 6 * 86_400 {
            return date.formatted(.dateTime.weekday(.abbreviated).hour().minute())
        }
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}
