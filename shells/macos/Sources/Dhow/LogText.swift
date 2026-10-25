import DhowCore
import Foundation

/// The detail pane's log lines (UI-GUIDELINES §5.6) and its warnings: what
/// happened, then why where the core says (§9).
enum LogText {
    static func line(_ entry: LogEntry) -> String {
        let time = entry.at.formatted(date: .omitted, time: .standard)
        return tr("log.line \(time) \(text(entry.kind))")
    }

    static func text(_ kind: LogEntry.Kind) -> String {
        switch kind {
        case .added: tr("log.added")
        case .state(let state, let reason): self.state(state, reason)
        case .retrying(let attempt, let delayMs):
            retrying(attempt, delay: Format.duration(seconds: (delayMs + 999) / 1000))
        case .reset(let reason): reset(reason)
        case .warning(let kind): warning(kind)
        case .renamed(let path): tr("log.renamed \((path as NSString).lastPathComponent)")
        }
    }

    static func retrying(_ attempt: UInt32, delay: String) -> String {
        tr("log.retrying \(Int(attempt)) \(delay)")
    }

    static func state(_ state: DownloadState, _ reason: FailReason?) -> String {
        switch state {
        case .queued: tr("log.queued")
        case .waiting: tr("log.waiting")
        case .active: tr("log.active")
        case .paused: tr("log.paused")
        case .completing: tr("log.completing")
        case .completed: tr("log.completed")
        case .failed: tr("log.failed \(RowStatus.failure(reason))")
        case .cancelled: tr("log.cancelled")
        }
    }

    static func reset(_ reason: ResetReason) -> String {
        switch reason {
        case .serverFileChanged: tr("reset.serverFileChanged")
        case .unaccountable: tr("reset.unaccountable")
        case .cannotResume: tr("reset.cannotResume")
        }
    }

    static func warning(_ kind: WarningKind) -> String {
        switch kind {
        case .notResumable: tr("warning.notResumable")
        case .sizeUnverified: tr("warning.sizeUnverified")
        case .identityWeak: tr("warning.identityWeak")
        case .serverIgnoredRange: tr("warning.serverIgnoredRange")
        }
    }
}
