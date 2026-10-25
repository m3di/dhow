import DhowCore
import Foundation

/// A row's status glyph and its one short phrase (UI-GUIDELINES §3).
struct RowStatus: Hashable {
    enum Glyph: String {
        case downloading = "arrow.down.circle"
        case reconnecting = "arrow.clockwise.circle"
        case finishing = "checkmark.circle.badge.questionmark"
        case waiting = "hourglass"
        case scheduled = "clock"
        case paused = "pause.circle"
        case failed = "exclamationmark.triangle"
        case cancelled = "xmark.circle"
        case done = "checkmark.circle.fill"
    }

    var glyph: Glyph
    var text: String
    /// Sorts the Status column: what moves first, finished last.
    var rank: Int

    static func of(
        _ d: DownloadSnapshot, live: Live?, queue: QueueSnapshot?, now: Date
    ) -> RowStatus {
        switch d.state {
        case .active:
            if let attempt = live?.reconnecting {
                return RowStatus(
                    glyph: .reconnecting, text: tr("status.reconnecting \(Int(attempt))"),
                    rank: 0)
            }
            if d.speedLimit == 0 {
                return RowStatus(glyph: .paused, text: tr("status.limitedToZero"), rank: 0)
            }
            if let eta = live?.etaSecs {
                return RowStatus(
                    glyph: .downloading, text: tr("status.left \(Format.duration(seconds: eta))"),
                    rank: 0)
            }
            return RowStatus(glyph: .downloading, text: tr("status.downloading"), rank: 0)
        case .completing:
            return RowStatus(glyph: .finishing, text: tr("status.finishing"), rank: 1)
        case .waiting:
            // The connection budget (ENGINE-SPEC §10.3).
            return RowStatus(glyph: .waiting, text: tr("status.waitingConnection"), rank: 2)
        case .queued:
            return queued(queue, now: now)
        case .paused:
            return RowStatus(glyph: .paused, text: tr("status.paused"), rank: 4)
        case .failed:
            return RowStatus(glyph: .failed, text: failure(d.failReason), rank: 5)
        case .cancelled:
            return RowStatus(glyph: .cancelled, text: tr("status.cancelled"), rank: 6)
        case .completed:
            return RowStatus(glyph: .done, text: tr("status.done"), rank: 7)
        }
    }

    /// Why a queued download hasn't started: its queue's override or
    /// schedule, then its concurrency (ARCHITECTURE §3.2).
    private static func queued(_ q: QueueSnapshot?, now: Date) -> RowStatus {
        guard let q else {
            return RowStatus(glyph: .waiting, text: tr("status.queued"), rank: 3)
        }
        if !q.activeNow {
            if q.runState == .stopped {
                if let until = q.runStateUntil {
                    return RowStatus(
                        glyph: .paused,
                        text: tr("status.stoppedUntil \(Format.time(unixMs: until, now: now))"),
                        rank: 3)
                }
                return RowStatus(glyph: .paused, text: tr("status.queueStopped"), rank: 3)
            }
            if let edge = q.nextEdge {
                return RowStatus(
                    glyph: .scheduled,
                    text: tr("status.scheduled \(Format.time(unixMs: edge, now: now))"), rank: 3)
            }
            return RowStatus(glyph: .scheduled, text: tr("status.notScheduled"), rank: 3)
        }
        if q.running >= q.maxConcurrent, q.running > 0 {
            return RowStatus(
                glyph: .waiting, text: tr("status.waitingFor \(Int(q.running))"), rank: 3)
        }
        return RowStatus(glyph: .waiting, text: tr("status.queued"), rank: 3)
    }

    /// The cause, in a few words; the detail pane says more.
    static func failure(_ reason: FailReason?) -> String {
        switch reason {
        case .transient, nil: tr("fail.transient")
        case .http(let status): tr("fail.http \(Int(status))")
        case .tooManyRedirects: tr("fail.tooManyRedirects")
        case .invalidRequest: tr("fail.invalidRequest")
        case .noSpace: tr("fail.noSpace")
        case .digestMismatch: tr("fail.digestMismatch")
        case .serverSentExtraData: tr("fail.serverSentExtraData")
        case .serverKeptChanging: tr("fail.serverKeptChanging")
        case .io: tr("fail.io")
        }
    }
}
