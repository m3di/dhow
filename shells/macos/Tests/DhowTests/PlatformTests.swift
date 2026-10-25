import DhowCore
import Foundation
import Testing

@testable import Dhow

/// Counts assertions instead of making them.
final class FakePower: PowerAssertions, @unchecked Sendable {
    private let lock = NSLock()
    private var next: UInt32 = 0
    private(set) var held = Set<UInt32>()
    private(set) var created = 0

    func preventIdleSleep(reason: String) -> UInt32? {
        lock.withLock {
            next += 1
            created += 1
            held.insert(next)
            return next
        }
    }

    func release(_ id: UInt32) {
        _ = lock.withLock { held.remove(id) }
    }
}

/// An HTTP server on 127.0.0.1 serving `size` bytes for any path (no
/// ranges), for a download that completes without the test server.
final class BlobServer: @unchecked Sendable {
    let fd: Int32
    let port: UInt16

    init(size: Int) throws {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        fd = sock
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        let bound = withUnsafeMutablePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(sock, $0, len) == 0 && listen(sock, 8) == 0
                    && getsockname(sock, $0, &len) == 0
            }
        }
        guard bound else { throw POSIXError(.EADDRINUSE) }
        port = UInt16(bigEndian: addr.sin_port)
        Thread.detachNewThread {
            let body = [UInt8](repeating: 0x61, count: size)
            while true {
                let c = accept(sock, nil, nil)
                guard c >= 0 else { return }
                var buf = [UInt8](repeating: 0, count: 8192)
                let n = read(c, &buf, buf.count)
                let head = String(decoding: buf.prefix(max(0, n)), as: UTF8.self)
                let reply =
                    "HTTP/1.1 200 OK\r\nContent-Length: \(size)\r\n"
                    + "Content-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
                _ = reply.withCString { write(c, $0, strlen($0)) }
                if !head.hasPrefix("HEAD") {
                    _ = body.withUnsafeBytes { write(c, $0.baseAddress, size) }
                }
                close(c)
            }
        }
    }

    deinit {
        close(fd)
    }
}

/// The macOS platform adapter (M4.3, ARCHITECTURE §4, §6, §8): its
/// mappings, bookmarks and keep-awake, without touching the system's
/// state (no real assertions, Trash or settings).
@Suite struct PlatformTests {
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("dhow-platform-\(UUID().uuidString)")

    init() throws {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    func cleanUp() {
        try? FileManager.default.removeItem(at: dir)
    }

    @Test func systemProxySettingsMapToProxyConfig() {
        let direct = SystemProxy.config([:])
        #expect(direct.isDirect)
        let c = SystemProxy.config([
            kCFNetworkProxiesHTTPEnable as String: 1,
            kCFNetworkProxiesHTTPProxy as String: "proxy.lan",
            kCFNetworkProxiesHTTPPort as String: 3128,
            kCFNetworkProxiesHTTPSEnable as String: 1,
            kCFNetworkProxiesHTTPSProxy as String: "fd00::1",
            kCFNetworkProxiesHTTPSPort as String: 8443,
            // Configured but off: not used.
            kCFNetworkProxiesSOCKSEnable as String: 0,
            kCFNetworkProxiesSOCKSProxy as String: "socks.lan",
            kCFNetworkProxiesExceptionsList as String: ["*.local", "169.254/16"],
            kCFNetworkProxiesExcludeSimpleHostnames as String: 1,
            kCFNetworkProxiesProxyAutoConfigEnable as String: 1,
            kCFNetworkProxiesProxyAutoConfigURLString as String: "http://wpad.lan/proxy.pac",
        ])
        #expect(c.http == "http://proxy.lan:3128")
        #expect(c.https == "http://[fd00::1]:8443")
        #expect(c.all == nil)
        #expect(c.noProxy == ["*.local", "169.254/16", "localhost"])
        #expect(c.pac == "http://wpad.lan/proxy.pac")
        let socks = SystemProxy.config([
            kCFNetworkProxiesSOCKSEnable as String: 1,
            kCFNetworkProxiesSOCKSProxy as String: "socks.lan",
            kCFNetworkProxiesSOCKSPort as String: 1080,
        ])
        #expect(socks.all == "socks5://socks.lan:1080")
        #expect(!socks.isDirect)
    }

    @Test func pathsMapToNetworkInfo() {
        typealias I = NetworkFacts.Interface
        func facts(_ i: [I]) -> NetworkFacts {
            NetworkFacts.from(interfaces: i, expensive: false, constrained: false)
        }
        #expect(facts([I(kind: .wifi, name: "en0")]).kind == .wifi)
        #expect(
            facts([I(kind: .loopback, name: "lo0"), I(kind: .ethernet, name: "en5")]).kind
                == .ethernet)
        #expect(facts([I(kind: .cellular, name: "pdp_ip0")]).kind == .cellular)
        // The default route through a tunnel: a VPN.
        let vpn = facts([I(kind: .other, name: "utun3"), I(kind: .wifi, name: "en0")])
        #expect(vpn.kind == .vpn && vpn.vpn)
        // A tunnel that isn't the default route: still a VPN active.
        let split = facts([I(kind: .wifi, name: "en0"), I(kind: .other, name: "ipsec0")])
        #expect(split.kind == .wifi && split.vpn)
        // Another kind of "other" (a bridge) isn't a VPN.
        #expect(!facts([I(kind: .other, name: "bridge0")]).vpn)
        #expect(facts([]).kind == .unknown)
        let metered = NetworkFacts.from(
            interfaces: [I(kind: .wifi, name: "en0")], expensive: true, constrained: true)
        #expect(metered.expensive && metered.constrained)
    }

    /// A bookmark follows its file when it moves, and calls a file in a
    /// Trash or deleted gone (ARCHITECTURE §6). The Trash here is a
    /// folder named like one, not the user's.
    @Test func bookmarksFollowMovesAndTreatTheTrashAsGone() throws {
        defer { cleanUp() }
        let fm = FileManager.default
        let file = dir.appendingPathComponent("movie.mkv")
        try Data("x".utf8).write(to: file)
        let adapter = MacAdapter(power: KeepAwake(power: FakePower()), proxies: { [:] })
        let token = try #require(try adapter.fileTokenCreate(path: file.path))

        let moved = dir.appendingPathComponent("Films/renamed.mkv")
        try fm.createDirectory(
            at: moved.deletingLastPathComponent(), withIntermediateDirectories: true)
        try fm.moveItem(at: file, to: moved)
        let found = try #require(try adapter.fileTokenResolve(token: token))
        #expect(
            URL(fileURLWithPath: found.path).resolvingSymlinksInPath()
                == moved.resolvingSymlinksInPath())

        let trash = dir.appendingPathComponent(".Trash/renamed.mkv")
        try fm.createDirectory(
            at: trash.deletingLastPathComponent(), withIntermediateDirectories: true)
        try fm.moveItem(at: moved, to: trash)
        #expect(try adapter.fileTokenResolve(token: token) == nil)

        try fm.removeItem(at: trash)
        #expect(try adapter.fileTokenResolve(token: token) == nil)
        #expect(Bookmarks.inTrash("/Volumes/Disk/.Trashes/501/a.iso"))
        #expect(!Bookmarks.inTrash("/Users/x/Downloads/a.iso"))
    }

    @Test func keepAwakeHoldsOneAssertionWhileAnyTransferRuns() {
        let power = FakePower()
        do {
            let awake = KeepAwake(power: power, reason: "test")
            awake.set(true)
            awake.set(true)
            #expect(power.created == 1 && power.held.count == 1)
            awake.set(false)
            #expect(awake.isHeld)
            awake.set(false)
            #expect(!awake.isHeld && power.held.isEmpty)
            // An extra off doesn't go negative.
            awake.set(false)
            awake.set(true)
            #expect(power.created == 2 && power.held.count == 1)
        }
        // Dropped while held (a quit mid-transfer): released.
        #expect(power.held.isEmpty)
    }

    /// End to end: a completed download's file moves, and `get` finds it
    /// where it went; moved to a Trash, it's missing.
    @Test func aMovedDownloadIsFoundWhereItWent() async throws {
        defer { cleanUp() }
        let server = try BlobServer(size: 4096)
        let power = FakePower()
        let adapter = MacAdapter(power: KeepAwake(power: power), proxies: { [:] })
        let data = dir.appendingPathComponent("data")
        let engine = try Engine.open(
            dataDir: data.path, config: EngineConfig(), listener: Listener(), adapter: adapter)
        let added = try await engine.add(
            request: AddRequest(
                url: "http://127.0.0.1:\(server.port)/paper.pdf", directory: dir.path,
                start: .now))
        for _ in 0..<500 where try engine.get(id: added.id).state != .completed {
            try await Task.sleep(for: .milliseconds(10))
        }
        let done = try engine.get(id: added.id)
        #expect(done.state == .completed)
        // The run held the Mac awake, and let go.
        #expect(power.created >= 1 && power.held.isEmpty)

        let elsewhere = dir.appendingPathComponent("Archive/paper.pdf")
        try FileManager.default.createDirectory(
            at: elsewhere.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.moveItem(atPath: done.finalPath, toPath: elsewhere.path)
        let found = try engine.get(id: added.id)
        #expect(
            URL(fileURLWithPath: found.finalPath).resolvingSymlinksInPath()
                == elsewhere.resolvingSymlinksInPath())

        let trash = dir.appendingPathComponent(".Trash/paper.pdf")
        try FileManager.default.createDirectory(
            at: trash.deletingLastPathComponent(), withIntermediateDirectories: true)
        try FileManager.default.moveItem(at: elsewhere, to: trash)
        let usage = try engine.storageBreakdown()
        #expect(usage.flatMap(\.missing) == [added.id])
        await engine.shutdown()
    }
}
