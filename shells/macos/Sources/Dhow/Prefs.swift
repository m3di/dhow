import Foundation

/// The shell's own settings (UI-GUIDELINES §7: General, Notifications,
/// Advanced), in `UserDefaults`. They apply as they change; the core's go
/// through the Settings window's Save.
enum Prefs {
    static let defaultFolder = "defaultFolder"
    static let askWhereToSave = "askWhereToSave"
    /// A queue id; the default queue when unset or gone.
    static let defaultQueue = "defaultQueue"
    /// Clicking a completion notification opens the file (else shows it).
    static let openFromNotification = "openFromNotification"
    static let notifyCompleted = "notifyCompleted"
    static let notifyFailed = "notifyFailed"
    static let notifyGroups = "notifyGroups"
    static let notifyQueues = "notifyQueues"
    static let notifySound = "notifySound"
    /// `LogLevel` raw value; read when the engine opens.
    static let logLevel = "logLevel"
    /// Off: the detail pane shows a plain bar and watches no ranges.
    static let showSegmentMap = "showSegmentMap"
    static let inspectorShown = "inspectorShown"

    static func register(_ defaults: UserDefaults = .standard) {
        defaults.register(defaults: [
            openFromNotification: true,
            notifyCompleted: true,
            notifyFailed: true,
            notifyGroups: true,
            notifyQueues: true,
            notifySound: true,
            showSegmentMap: true,
            inspectorShown: true,
            logLevel: LogLevel.standard.rawValue,
        ])
    }
}

/// The log's detail (ARCHITECTURE §5.3): an EnvFilter for `init_logging`.
enum LogLevel: String, CaseIterable {
    case errors, standard, detailed

    var filter: String {
        switch self {
        case .errors: "error"
        case .standard: "warn,dhow=info"
        case .detailed: "info,dhow=debug"
        }
    }
}
