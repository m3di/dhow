import DhowCore
import Foundation
import Testing

@testable import Dhow

/// The detail pane's controls against a real engine (UI-GUIDELINES §5.5).
@MainActor
@Suite struct DetailCommandTests {
    func until(_ what: String, _ ok: () throws -> Bool) async throws {
        for _ in 0..<500 where try !ok() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(try ok(), "\(what)")
    }

    /// Nothing listens on port 1: added blind, it can't fetch a byte.
    func add(_ core: TempEngine, _ start: StartMode) async throws -> String {
        try await core.engine.add(
            request: AddRequest(
                url: "http://127.0.0.1:1/x", directory: core.dir.path, start: start, blind: true)
        ).id
    }

    @Test func quickStepsReachTheEngineInOrder() async throws {
        let model = AppModel()
        let core = try TempEngine(listener: model.listener)
        try await model.attach(core.engine)
        let id = try await add(core, .paused)
        try await until("seen") { model.store.downloads[id] != nil }
        for n: UInt8 in [2, 3, 4, 5, 6, 7] {
            model.setConnections(id, n)
            model.setSpeedLimit([id], UInt64(n) * 1000)
        }
        try await until("applied") { model.store.downloads[id]?.connections == 7 }
        #expect(try core.engine.get(id: id).connections == 7)
        #expect(try core.engine.get(id: id).speedLimit == 7000)
        #expect(model.store.downloads[id]?.speedLimit == 7000)
        model.setConnections(id, nil)
        try await until("default") { model.store.downloads[id]?.connections == nil }
        #expect(try core.engine.get(id: id).connections == nil)
        await core.close()
    }

    /// The pane's Retry puts a failed download back in its queue, which
    /// starts it.
    @Test func retryRequeuesAFailedDownload() async throws {
        let model = AppModel()
        let core = try TempEngine(listener: model.listener)
        try await model.attach(core.engine)
        let server = try NotFoundServer()
        let id = try await core.engine.add(
            request: AddRequest(
                url: "http://127.0.0.1:\(server.port)/x", directory: core.dir.path,
                start: .queued, blind: true)
        ).id
        try await until("failed") { model.store.downloads[id]?.state == .failed }
        let before = model.store.logs[id]?.count ?? 0
        model.retry(id)
        // Queued, started by its queue, and (the server still says 404)
        // failed again: the log has it all.
        try await until("ran again") {
            let after = (model.store.logs[id] ?? []).dropFirst(before).map(\.kind)
            return after.first == .state(.queued, nil) && after.contains(.state(.active, nil))
        }
        await core.close()
    }

    /// An HTTP server on 127.0.0.1 answering every request 404, so a
    /// download fails for good (ENGINE-SPEC §9) without the test server.
    final class NotFoundServer: @unchecked Sendable {
        let fd: Int32
        let port: UInt16

        init() throws {
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
                while true {
                    let c = accept(sock, nil, nil)
                    guard c >= 0 else { return }
                    var buf = [UInt8](repeating: 0, count: 4096)
                    _ = read(c, &buf, buf.count)
                    let reply =
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    _ = reply.withCString { write(c, $0, strlen($0)) }
                    close(c)
                }
            }
        }

        deinit {
            close(fd)
        }
    }

    @Test func limitStepsAgreeWithTheLabel() {
        let steps = LimitSteps.steps(for: nil)
        #expect(steps.last! == nil)
        #expect(LimitSteps.index(of: nil, in: steps) == steps.count - 1)
        #expect(LimitSteps.index(of: 256_000, in: steps) == 2)
        // One the CLI set between steps sits at the nearest.
        #expect(LimitSteps.index(of: 300_000, in: steps) == 2)
        #expect(LimitSteps.label(256_000) == Format.speed(256_000))
        // Held by its limit: a step of its own, named as the list names it.
        let held = LimitSteps.steps(for: 0)
        #expect(held.first! == 0)
        #expect(LimitSteps.index(of: 0, in: held) == 0)
        #expect(LimitSteps.label(0) == tr("status.limitedToZero"))
        #expect(!LimitSteps.steps(for: 64_000).contains(0))
    }
}
