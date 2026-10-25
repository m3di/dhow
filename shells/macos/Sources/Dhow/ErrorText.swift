import DhowCore

/// The core's errors are diagnostics (ARCHITECTURE §9); these are the words
/// users see: the cause, then the next step (UI-GUIDELINES §9).
enum ErrorText {
    static func message(_ error: Error) -> String {
        guard let error = error as? DhowError else {
            return tr("error.unexpected")
        }
        switch error {
        case .Probe(let reason): return probe(reason)
        case .NotFound: return tr("error.notFound")
        case .InvalidName: return tr("error.invalidName")
        case .InvalidArgument: return tr("error.invalidArgument")
        case .InvalidQueue, .DefaultQueue: return tr("error.invalidQueue")
        case .InvalidSetting: return tr("error.invalidSetting")
        case .DatabaseTooNew: return tr("error.databaseTooNew")
        case .Storage, .Io: return tr("error.storage")
        case .RunningElsewhere: return tr("error.runningElsewhere")
        case .Adapter, .Internal: return tr("error.unexpected")
        case .ShutDown: return tr("error.shutDown")
        }
    }

    static func probe(_ reason: ProbeFailure) -> String {
        switch reason {
        case .unsupportedScheme: tr("probe.unsupportedScheme")
        case .invalidHeader: tr("probe.invalidHeader")
        case .status(let status): tr("probe.status \(Int(status))")
        case .tooManyRedirects: tr("probe.tooManyRedirects")
        case .timeout: tr("probe.timeout")
        case .connect: tr("probe.connect")
        case .network: tr("probe.network")
        }
    }

    /// Whether "Download anyway" can help: blind mode never covers a bad
    /// scheme, header or URL (DECISIONS 2026-10-25 "Shell commands").
    static func blindCanHelp(_ error: Error) -> Bool {
        guard case DhowError.Probe(let reason) = error else { return false }
        return reason != .unsupportedScheme && reason != .invalidHeader
    }
}
