import DhowCore
import Foundation
import Observation

/// The Settings window's fields (UI-GUIDELINES §7) in the units it shows:
/// kB/s (1000 bytes, as `Format` shows them), seconds, lists as text. Every
/// `DhowSettings` field, plus the shell's default folder.
struct SettingsDraft: Equatable {
    enum Proxy: Hashable { case system, direct, custom }

    /// A value its setting's type can't hold (a negative count, 300 for a
    /// byte): said at the field, never clamped into another value.
    struct OutOfRange: Error, Equatable {
        var key: String
    }

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
        limitKB = Int(clamping: (s.globalSpeedLimit ?? 1_000_000) / 1000)
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

    /// What differs from `s`, as one patch; the core says whether the
    /// values are valid. Throws for one its type can't hold.
    func patch(over s: DhowSettings) throws(OutOfRange) -> SettingsPatch {
        var p = SettingsPatch()
        func changed<T: Equatable>(_ new: T, _ old: T) -> T? { new == old ? nil : new }
        func fit<T: FixedWidthInteger>(_ v: Int, _ key: String) throws(OutOfRange) -> T {
            guard let x = T(exactly: v) else { throw OutOfRange(key: key) }
            return x
        }
        // Compared as shown: a limit the CLI set in bytes stays unless edited.
        let shown = SettingsDraft(s, folder: "")
        if limitOn != shown.limitOn || limitKB != shown.limitKB {
            if limitOn {
                let kb: UInt64 = try fit(limitKB, "global_speed_limit")
                let (bytes, overflow) = kb.multipliedReportingOverflow(by: 1000)
                guard !overflow else { throw OutOfRange(key: "global_speed_limit") }
                p.globalSpeedLimit = .rate(bytesPerSec: bytes)
            } else {
                p.globalSpeedLimit = .unlimited
            }
        }
        p.headroomPercent = changed(try fit(headroomPercent, "headroom_percent"), s.headroomPercent)
        p.globalConnections = changed(
            try fit(globalConnections, "global_connections"), s.globalConnections)
        p.perHostConnections = changed(
            try fit(perHostConnections, "per_host_connections"), s.perHostConnections)
        p.minConnectionsForNew = changed(newcomerGetsConnection ? 1 : 0, s.minConnectionsForNew)
        p.defaultConnections = changed(
            try fit(defaultConnections, "default_connections"), s.defaultConnections)
        p.maxConnectionsPerDownload = changed(
            try fit(maxConnectionsPerDownload, "max_connections_per_download"),
            s.maxConnectionsPerDownload)
        p.firstByteTimeoutMs = changed(
            try Self.ms(firstByteTimeout, "first_byte_timeout_ms"), s.firstByteTimeoutMs)
        p.idleTimeoutMs = changed(try Self.ms(idleTimeout, "idle_timeout_ms"), s.idleTimeoutMs)
        p.maxRetries = changed(try fit(maxRetries, "max_retries"), s.maxRetries)
        p.commitIntervalMs = changed(
            try Self.ms(commitInterval, "commit_interval_ms"), s.commitIntervalMs)
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

    /// The fields edited since `old`, over `new`: what another process
    /// changed meanwhile comes in, the user's edits stay.
    func rebased(from old: SettingsDraft, to new: SettingsDraft) -> SettingsDraft {
        var d = self
        func follow<T: Equatable>(_ field: WritableKeyPath<SettingsDraft, T>) {
            if d[keyPath: field] == old[keyPath: field] {
                d[keyPath: field] = new[keyPath: field]
            }
        }
        follow(\.defaultFolder)
        follow(\.limitOn)
        follow(\.limitKB)
        follow(\.headroomPercent)
        follow(\.globalConnections)
        follow(\.perHostConnections)
        follow(\.newcomerGetsConnection)
        follow(\.defaultConnections)
        follow(\.maxConnectionsPerDownload)
        follow(\.firstByteTimeout)
        follow(\.idleTimeout)
        follow(\.maxRetries)
        follow(\.commitInterval)
        follow(\.probeRangeGet)
        follow(\.useContentDisposition)
        follow(\.appendTypeExtension)
        follow(\.autoGrouping)
        for c in DhowCore.Category.allCases {
            follow(\.extraExtensions[c])
        }
        follow(\.proxy)
        follow(\.proxyHttp)
        follow(\.proxyHttps)
        follow(\.proxyOther)
        follow(\.noProxy)
        return d
    }

    /// Seconds as milliseconds; one that isn't a time is refused here.
    static func ms(_ seconds: Double, _ key: String) throws(OutOfRange) -> UInt64 {
        let ms = (seconds * 1000).rounded()
        guard seconds.isFinite, ms >= 0, ms < 0x1p63 else { throw OutOfRange(key: key) }
        return UInt64(ms)
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
/// error of the last save. A save sends one `set_settings` patch with the
/// user's edits, which the core validates whole (ARCHITECTURE §3.1). The
/// app keeps it, so edits survive closing the window, unsaved.
@MainActor @Observable
final class SettingsForm {
    struct Failure: Equatable {
        /// The setting it names, as the core's key; `nil`: none.
        var key: String?
        var message: String
        /// For `extra_extensions`: the categories the refused patch changed
        /// (the core names the setting, not the category).
        var categories: Set<DhowCore.Category> = []
    }

    var draft = SettingsDraft()
    private(set) var base: DhowSettings?
    private(set) var baseFolder = ""
    /// `base` as the draft shows it: what edits are measured from.
    private var origin = SettingsDraft()
    private(set) var failure: Failure?
    private(set) var saving = false

    /// Something to save: the patch isn't empty (or can't be made), or the
    /// folder changed.
    var isDirty: Bool {
        guard let base else { return false }
        if draft.defaultFolder != baseFolder {
            return true
        }
        guard let patch = try? draft.patch(over: base) else { return true }
        return patch != SettingsPatch()
    }

    /// What the core has now. Fields the user hasn't edited follow it;
    /// edited ones stay.
    func load(_ settings: DhowSettings, folder: String) {
        let shown = SettingsDraft(settings, folder: folder)
        draft = base == nil ? shown : draft.rebased(from: origin, to: shown)
        origin = shown
        base = settings
        baseFolder = folder
    }

    func revert() {
        failure = nil
        draft = origin
    }

    func error(for key: String) -> String? {
        failure?.key == key ? failure?.message : nil
    }

    func error(for category: DhowCore.Category) -> String? {
        failure?.categories.contains(category) == true ? failure?.message : nil
    }

    /// Sends the patch; on success the folder is stored too and the draft
    /// is what the core now has. Returns the tab to show an error on.
    @discardableResult
    func save(
        _ send: (SettingsPatch) async throws -> DhowSettings?, folder store: (String) -> Void
    ) async -> SettingsTab? {
        guard let base, !saving else { return nil }
        let patch: SettingsPatch
        do {
            patch = try draft.patch(over: base)
        } catch {
            failure = Failure(key: error.key, message: SettingsText.invalid(error.key, error))
            return SettingsTab.of(key: error.key)
        }
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
            origin = SettingsDraft(stored, folder: folder)
            draft = origin
            return nil
        } catch {
            let key = Self.key(of: error, patch: patch)
            failure = Failure(
                key: key, message: SettingsText.invalid(key, error),
                categories: key == "extra_extensions"
                    ? Set(patch.extraExtensions.map(\.category)) : [])
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
        case "global_speed_limit": tr("settings.invalid.limit")
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
