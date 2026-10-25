import DhowCore
import Foundation

/// The core's view of the Mac (ARCHITECTURE §4, §6, §8): file tokens,
/// the system proxy, the network, keep-awake. The engine calls it on its
/// own threads and each method may block; none hops to the main thread
/// or calls the engine (the network monitor does that, outside these
/// calls). Holds no engine.
final class MacAdapter: PlatformAdapter, @unchecked Sendable {
    let network: NetworkMonitor
    let power: KeepAwake
    private let proxies: () -> [String: Any]?

    init(
        network: NetworkMonitor = NetworkMonitor(), power: KeepAwake = KeepAwake(),
        proxies: @escaping () -> [String: Any]? = SystemProxy.current
    ) {
        self.network = network
        self.power = power
        self.proxies = proxies
    }

    func fileTokenCreate(path: String) throws -> Data? {
        do {
            return try Bookmarks.create(path)
        } catch {
            throw AdapterError.Failed(message: "bookmark: \(error.localizedDescription)")
        }
    }

    func fileTokenResolve(token: Data) throws -> ResolvedFile? {
        switch Bookmarks.resolve(token) {
        case .gone: return nil
        case .unknown(let why): throw AdapterError.Failed(message: why)
        case .found(let path, let refreshed): return ResolvedFile(path: path, token: refreshed)
        }
    }

    /// The system's settings apply to every URL alike (a PAC script is
    /// reported, not run: ARCHITECTURE §8).
    func systemProxy(forUrl: String) throws -> ProxyConfig {
        guard let settings = proxies() else {
            throw AdapterError.Failed(message: "no system proxy settings")
        }
        return SystemProxy.config(settings)
    }

    func networkInfo() throws -> AdapterNetworkInfo {
        let facts = network.current
        return AdapterNetworkInfo(
            interfaceKind: facts?.kind ?? .unknown, vpnActive: facts?.vpn ?? false,
            systemProxy: proxies().map(SystemProxy.config).flatMap { $0.isDirect ? nil : $0 })
    }

    /// Called with an engine lock held: only an assertion, no engine.
    func keepAwake(on: Bool) {
        power.set(on)
    }

    /// A Mac app runs as long as it's open.
    func backgroundCapability() -> BackgroundCapability {
        .unrestricted
    }
}

extension ProxyConfig {
    /// No proxy for any scheme, no PAC: a direct connection.
    var isDirect: Bool { http == nil && https == nil && all == nil && pac == nil }
}

/// File tokens (ARCHITECTURE §6): bookmarks, plain ones as Dhow isn't
/// sandboxed (security-scoped ones come with the sandbox).
enum Bookmarks {
    enum Resolution: Equatable {
        /// Where it is now, and a fresh token if the old one went stale.
        case found(path: String, refreshed: Data?)
        /// Deleted, or in a Trash: gone for good.
        case gone
        /// Can't tell now (a volume not mounted): ask again later.
        case unknown(String)
    }

    static func create(_ path: String) throws -> Data {
        try URL(fileURLWithPath: path).bookmarkData(
            options: [], includingResourceValuesForKeys: nil, relativeTo: nil)
    }

    static func resolve(_ token: Data) -> Resolution {
        var stale = false
        let url: URL
        do {
            // Never mounts a volume or shows UI from an engine thread.
            url = try URL(
                resolvingBookmarkData: token, options: [.withoutUI, .withoutMounting],
                relativeTo: nil, bookmarkDataIsStale: &stale)
        } catch let error as CocoaError where error.code == .fileNoSuchFile {
            // Not found: deleted, or on a volume that isn't mounted, which
            // can't tell now (ARCHITECTURE §6), so the token must be kept.
            let stored = URL.resourceValues(forKeys: [.pathKey], fromBookmarkData: token)?.path
            return missing(stored, mounted: { FileManager.default.fileExists(atPath: $0) })
        } catch {
            return .unknown("bookmark: \((error as NSError).code)")
        }
        let path = url.path(percentEncoded: false)
        // Resolved, yet not there now (it went meanwhile): ask again later.
        guard FileManager.default.fileExists(atPath: path) else {
            return .unknown("resolved to a missing path")
        }
        // A file moved to the Trash is gone for good (ARCHITECTURE §6).
        let volume = (try? url.resourceValues(forKeys: [.volumeURLKey]))?.volume?
            .path(percentEncoded: false)
        if inTrash(path, home: NSHomeDirectory(), volume: volume) {
            return .gone
        }
        let refreshed = stale ? try? create(path) : nil
        return .found(path: path, refreshed: refreshed)
    }

    /// A bookmark that found nothing: gone, unless the file was on a volume
    /// whose mount point isn't there (an unplugged disk).
    static func missing(_ storedPath: String?, mounted: (String) -> Bool) -> Resolution {
        guard let storedPath, let root = volumeRoot(storedPath) else { return .gone }
        return mounted(root) ? .gone : .unknown("volume \(root) not mounted")
    }

    /// `/Volumes/Name` for a path on a volume mounted there; `nil` for the
    /// startup volume.
    static func volumeRoot(_ path: String) -> String? {
        let parts = URL(fileURLWithPath: path).pathComponents
        guard parts.count >= 3, parts[0] == "/", parts[1] == "Volumes" else { return nil }
        return "/Volumes/" + parts[2]
    }

    /// In the user's Trash (`~/.Trash/`) or the Trash of the volume it's on
    /// (`<volume>/.Trashes/`); a folder that's merely named so isn't.
    static func inTrash(_ path: String, home: String, volume: String?) -> Bool {
        func under(_ dir: String) -> Bool {
            path.hasPrefix(dir.hasSuffix("/") ? dir : dir + "/")
        }
        if under((home as NSString).appendingPathComponent(".Trash")) {
            return true
        }
        guard let volume, volume != "/" else { return false }
        return under((volume as NSString).appendingPathComponent(".Trashes"))
    }
}

/// The system's proxy settings (`CFNetworkCopySystemProxySettings`) as the
/// core's `ProxyConfig`. URLs carry no credentials: macOS keeps those in
/// the keychain, not in these settings, and none are logged.
enum SystemProxy {
    static func current() -> [String: Any]? {
        CFNetworkCopySystemProxySettings()?.takeRetainedValue() as? [String: Any]
    }

    /// A field that can't make a URL (a scheme in the host, an IPv6 zone,
    /// a port out of range) is left out, not the whole answer.
    static func config(_ s: [String: Any]) -> ProxyConfig {
        func proxy(_ enable: String, _ host: String, _ port: String, scheme: String) -> String? {
            guard (s[enable] as? NSNumber)?.boolValue == true,
                let name = (s[host] as? String)?.trimmingCharacters(in: .whitespaces)
            else { return nil }
            return url(scheme: scheme, host: name, port: (s[port] as? NSNumber)?.intValue)
        }
        let noProxy = ((s[kCFNetworkProxiesExceptionsList as String] as? [String]) ?? []).map(
            fullCIDR)
        let pac =
            (s[kCFNetworkProxiesProxyAutoConfigEnable as String] as? NSNumber)?.boolValue == true
            ? s[kCFNetworkProxiesProxyAutoConfigURLString as String] as? String : nil
        return ProxyConfig(
            http: proxy(
                kCFNetworkProxiesHTTPEnable as String, kCFNetworkProxiesHTTPProxy as String,
                kCFNetworkProxiesHTTPPort as String, scheme: "http"),
            // An HTTPS proxy is reached with CONNECT over plain HTTP.
            https: proxy(
                kCFNetworkProxiesHTTPSEnable as String, kCFNetworkProxiesHTTPSProxy as String,
                kCFNetworkProxiesHTTPSPort as String, scheme: "http"),
            // `socks5h`: names resolve at the proxy, as CFNetwork does.
            all: proxy(
                kCFNetworkProxiesSOCKSEnable as String, kCFNetworkProxiesSOCKSProxy as String,
                kCFNetworkProxiesSOCKSPort as String, scheme: "socks5h"),
            noProxy: noProxy, pac: pac?.isEmpty == false ? pac : nil,
            // "Exclude simple hostnames": names without a dot go direct.
            excludeSimpleHosts: (s[kCFNetworkProxiesExcludeSimpleHostnames as String] as? NSNumber)?
                .boolValue
                == true)
    }

    /// macOS's short IPv4 networks ("169.254/16") written in full
    /// ("169.254.0.0/16"), as the core's `no_proxy` reads them; anything
    /// else as is.
    static func fullCIDR(_ entry: String) -> String {
        let parts = entry.split(separator: "/", omittingEmptySubsequences: false)
        guard parts.count == 2, Int(parts[1]) != nil else { return entry }
        let octets = parts[0].split(separator: ".", omittingEmptySubsequences: false)
        guard (1...4).contains(octets.count),
            octets.allSatisfy({ UInt8($0) != nil })
        else { return entry }
        let full = octets.map(String.init) + Array(repeating: "0", count: 4 - octets.count)
        return full.joined(separator: ".") + "/" + parts[1]
    }

    /// `scheme://host[:port]`, or `nil` when the parts can't make one.
    static func url(scheme: String, host: String, port: Int?) -> String? {
        guard !host.isEmpty, !host.contains("://"), !host.contains("%"),
            !host.contains("/"), !host.contains("@"), !host.contains(" ")
        else { return nil }
        let bracketed = host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
        var text = "\(scheme)://\(bracketed)"
        if let port {
            guard (1...65535).contains(port) else { return nil }
            text += ":\(port)"
        }
        guard let parsed = URL(string: text), parsed.host != nil else { return nil }
        return text
    }
}
