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
            return .gone
        } catch {
            return .unknown("bookmark: \((error as NSError).code)")
        }
        let path = url.path(percentEncoded: false)
        guard FileManager.default.fileExists(atPath: path) else {
            return .gone
        }
        // A file moved to the Trash is gone for good (ARCHITECTURE §6).
        if inTrash(path) {
            return .gone
        }
        let refreshed = stale ? try? create(path) : nil
        return .found(path: path, refreshed: refreshed)
    }

    /// In the user's Trash (`~/.Trash`) or a volume's (`.Trashes`).
    static func inTrash(_ path: String) -> Bool {
        URL(fileURLWithPath: path).pathComponents.contains { $0 == ".Trash" || $0 == ".Trashes" }
    }
}

/// The system's proxy settings (`CFNetworkCopySystemProxySettings`) as the
/// core's `ProxyConfig`. URLs carry no credentials: macOS keeps those in
/// the keychain, not in these settings, and none are logged.
enum SystemProxy {
    static func current() -> [String: Any]? {
        CFNetworkCopySystemProxySettings()?.takeRetainedValue() as? [String: Any]
    }

    static func config(_ s: [String: Any]) -> ProxyConfig {
        func proxy(_ enable: String, _ host: String, _ port: String, scheme: String) -> String? {
            guard (s[enable] as? NSNumber)?.boolValue == true,
                let name = (s[host] as? String)?.trimmingCharacters(in: .whitespaces),
                !name.isEmpty
            else { return nil }
            let h = name.contains(":") && !name.hasPrefix("[") ? "[\(name)]" : name
            if let p = (s[port] as? NSNumber)?.intValue, p > 0 {
                return "\(scheme)://\(h):\(p)"
            }
            return "\(scheme)://\(h)"
        }
        var noProxy = (s[kCFNetworkProxiesExceptionsList as String] as? [String]) ?? []
        // "Exclude simple hostnames": names without a dot go direct.
        if (s[kCFNetworkProxiesExcludeSimpleHostnames as String] as? NSNumber)?.boolValue == true {
            noProxy.append("localhost")
        }
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
            all: proxy(
                kCFNetworkProxiesSOCKSEnable as String, kCFNetworkProxiesSOCKSProxy as String,
                kCFNetworkProxiesSOCKSPort as String, scheme: "socks5"),
            noProxy: noProxy, pac: pac?.isEmpty == false ? pac : nil)
    }
}
