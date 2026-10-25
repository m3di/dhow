import DhowCore
import Foundation
import Observation

/// The Settings window's fields (UI-GUIDELINES §7) in the units it shows:
/// KB/s, seconds, lists as text. Every `DhowSettings` field, plus the
/// shell's default folder.
struct SettingsDraft: Equatable {
    enum Proxy: Hashable { case system, direct, custom }

    var defaultFolder = ""
    var limitOn = false
    var limitKB = 0
    var headroomPercent = 0
    var globalConnections = 0
    var perHostConnections = 0
    var newcomerGetsConnection = true
    var defaultConnections = 0
    var maxConnectionsPerDownload = 0
    var firstByteTimeout = 0.0
    var idleTimeout = 0.0
    var maxRetries = 0
    var commitInterval = 0.0
    var probeRangeGet = true
    var useContentDisposition = true
    var appendTypeExtension = true
    var autoGrouping = true
    var extraExtensions: [DhowCore.Category: String] = [:]
    var proxy = Proxy.system
    var proxyHttp = ""
    var proxyHttps = ""
    var proxyOther = ""
    var noProxy = ""

    init() {}

    init(_ s: DhowSettings, folder: String) {
        defaultFolder = folder
        limitOn = s.globalSpeedLimit != nil
        limitKB = Int(clamping: (s.globalSpeedLimit ?? 1024 * 1024) / 1024)
        headroomPercent = Int(s.headroomPercent)
        globalConnections = Int(s.globalConnections)
        perHostConnections = Int(s.perHostConnections)
        newcomerGetsConnection = s.minConnectionsForNew > 0
        defaultConnections = Int(s.defaultConnections)
        maxConnectionsPerDownload = Int(s.maxConnectionsPerDownload)
        firstByteTimeout = Double(s.firstByteTimeoutMs) / 1000
        idleTimeout = Double(s.idleTimeoutMs) / 1000
        maxRetries = Int(s.maxRetries)
        commitInterval = Double(s.commitIntervalMs) / 1000
        probeRangeGet = s.probeRangeGet
        useContentDisposition = s.useContentDisposition
        appendTypeExtension = s.appendTypeExtension
        autoGrouping = s.autoGrouping
        for e in s.extraExtensions {
            extraExtensions[e.category] = e.extensions.joined(separator: ", ")
        }
        switch s.proxyMode {
        case .system: proxy = .system
        case .direct: proxy = .direct
        case .custom(let c):
            proxy = .custom
            proxyHttp = c.http ?? ""
            proxyHttps = c.https ?? ""
            proxyOther = c.all ?? ""
            noProxy = c.noProxy.joined(separator: ", ")
        }
    }

    /// What differs from `s`, as one patch; values are passed as typed (in
    /// range of their type), and the core says whether they are valid.
    func patch(over s: DhowSettings) -> SettingsPatch {
        var p = SettingsPatch()
        func changed<T: Equatable>(_ new: T, _ old: T) -> T? { new == old ? nil : new }
        // Compared as shown: a limit the CLI set in bytes stays unless edited.
        let shown = SettingsDraft(s, folder: "")
        if limitOn != shown.limitOn || limitKB != shown.limitKB {
            p.globalSpeedLimit =
                limitOn ? .rate(bytesPerSec: UInt64(clamping: limitKB) * 1024) : .unlimited
        }
        p.headroomPercent = changed(UInt8(clamping: headroomPercent), s.headroomPercent)
        p.globalConnections = changed(UInt32(clamping: globalConnections), s.globalConnections)
        p.perHostConnections = changed(
            UInt32(clamping: perHostConnections), s.perHostConnections)
        p.minConnectionsForNew = changed(newcomerGetsConnection ? 1 : 0, s.minConnectionsForNew)
        p.defaultConnections = changed(UInt8(clamping: defaultConnections), s.defaultConnections)
        p.maxConnectionsPerDownload = changed(
            UInt8(clamping: maxConnectionsPerDownload), s.maxConnectionsPerDownload)
        p.firstByteTimeoutMs = changed(Self.ms(firstByteTimeout), s.firstByteTimeoutMs)
        p.idleTimeoutMs = changed(Self.ms(idleTimeout), s.idleTimeoutMs)
        p.maxRetries = changed(UInt32(clamping: maxRetries), s.maxRetries)
        p.commitIntervalMs = changed(Self.ms(commitInterval), s.commitIntervalMs)
        p.probeRangeGet = changed(probeRangeGet, s.probeRangeGet)
        p.useContentDisposition = changed(useContentDisposition, s.useContentDisposition)
        p.appendTypeExtension = changed(appendTypeExtension, s.appendTypeExtension)
        p.autoGrouping = changed(autoGrouping, s.autoGrouping)
        let old = Dictionary(
            s.extraExtensions.map { ($0.category, $0.extensions) }, uniquingKeysWith: { a, _ in a })
        p.extraExtensions = DhowCore.Category.allCases.compactMap { c in
            let list = Self.list(extraExtensions[c] ?? "")
            return list == old[c, default: []]
                ? nil : CategoryExtensions(category: c, extensions: list)
        }
        p.proxyMode = changed(proxyMode, s.proxyMode)
        return p
    }

    var proxyMode: ProxyMode {
        switch proxy {
        case .system: .system
        case .direct: .direct
        case .custom:
            .custom(
                config: ProxyConfig(
                    http: Self.text(proxyHttp), https: Self.text(proxyHttps),
                    all: Self.text(proxyOther), noProxy: Self.list(noProxy), pac: nil))
        }
    }

    /// Seconds as whole milliseconds; below zero is 0, which the core
    /// refuses as it would any value out of range.
    static func ms(_ seconds: Double) -> UInt64 {
        guard seconds.isFinite, seconds > 0 else { return 0 }
        return UInt64(clamping: Int64((seconds * 1000).rounded()))
    }

    /// "mkv, webm .m4v" as ["mkv", "webm", ".m4v"]: the core normalizes.
    static func list(_ text: String) -> [String] {
        text.split(whereSeparator: { $0 == "," || $0.isWhitespace }).map(String.init)
    }

    private static func text(_ s: String) -> String? {
        let t = s.trimmingCharacters(in: .whitespaces)
        return t.isEmpty ? nil : t
    }
}

/// The tabs of the Settings window, in UI-GUIDELINES §7's order.
enum SettingsTab: Hashable, CaseIterable {
    case general, connections, speed, network, categories

    /// The tab showing a setting the core named in an error.
    static func of(key: String) -> SettingsTab? {
        switch key {
        case "global_connections", "per_host_connections", "min_connections_for_new",
            "default_connections", "max_connections_per_download", "first_byte_timeout_ms",
            "idle_timeout_ms", "max_retries", "probe_range_get":
            .connections
        case "global_speed_limit", "headroom_percent", "commit_interval_ms": .speed
        case "proxy_mode": .network
        case "use_content_disposition", "append_type_extension", "auto_grouping",
            "extra_extensions":
            .categories
        default: nil
        }
    }
}

/// The Settings window's state: the draft, what the core has, and the
/// error of the last save. A save sends one `set_settings` patch with
/// every change, which the core validates whole (ARCHITECTURE §3.1).
@MainActor @Observable
final class SettingsForm {
    struct Failure: Equatable {
        /// The setting it names, as the core's key; `nil`: none.
        var key: String?
        var message: String
    }

    var draft = SettingsDraft()
    private(set) var base: DhowSettings?
    private(set) var baseFolder = ""
    private(set) var failure: Failure?
    private(set) var saving = false

    var isDirty: Bool {
        guard let base else { return false }
        return draft != SettingsDraft(base, folder: baseFolder)
    }

    /// What the core has now; the draft follows unless it has edits.
    func load(_ settings: DhowSettings, folder: String) {
        let follow = !isDirty
        base = settings
        baseFolder = folder
        if follow {
            draft = SettingsDraft(settings, folder: folder)
        }
    }

    func revert() {
        failure = nil
        if let base {
            draft = SettingsDraft(base, folder: baseFolder)
        }
    }

    func error(for key: String) -> String? {
        failure?.key == key ? failure?.message : nil
    }

    /// Sends the patch; on success the folder is stored too and the draft
    /// is what the core now has. Returns the tab to show an error on.
    @discardableResult
    func save(
        _ send: (SettingsPatch) async throws -> DhowSettings?, folder store: (String) -> Void
    ) async -> SettingsTab? {
        guard let base, !saving else { return nil }
        let patch = draft.patch(over: base)
        let folder = draft.defaultFolder
        saving = true
        defer { saving = false }
        do {
            var stored = base
            if patch != SettingsPatch() {
                stored = try await send(patch) ?? base
            }
            if folder != baseFolder {
                store(folder)
            }
            failure = nil
            self.base = stored
            baseFolder = folder
            draft = SettingsDraft(stored, folder: folder)
            return nil
        } catch {
            let key = Self.key(of: error, patch: patch)
            failure = Failure(key: key, message: SettingsText.invalid(key, error))
            return key.flatMap(SettingsTab.of)
        }
    }

    /// The setting an error is about: the one the core names, or the proxy
    /// when its URLs don't parse (`InvalidArgument`, before validation).
    static func key(of error: Error, patch: SettingsPatch) -> String? {
        switch error as? DhowError {
        case .InvalidSetting(let key, _): key
        case .InvalidArgument where patch.proxyMode != nil: "proxy_mode"
        default: nil
        }
    }
}

/// The words for a setting the core refused: what it accepts.
enum SettingsText {
    static func invalid(_ key: String?, _ error: Error) -> String {
        switch key {
        case "headroom_percent": tr("settings.invalid.headroom")
        case "global_connections", "per_host_connections": tr("settings.invalid.atLeastOne")
        case "min_connections_for_new": tr("settings.invalid.newcomer")
        case "default_connections": tr("settings.invalid.defaultConnections")
        case "max_connections_per_download": tr("settings.invalid.maxConnections")
        case "first_byte_timeout_ms", "idle_timeout_ms": tr("settings.invalid.timeout")
        case "max_retries": tr("settings.invalid.atLeastOne")
        case "commit_interval_ms": tr("settings.invalid.commit")
        case "proxy_mode": tr("settings.invalid.proxy")
        case "extra_extensions": tr("settings.invalid.extensions")
        default: ErrorText.message(error)
        }
    }
}
