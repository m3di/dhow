import DhowCore
import Foundation
import Testing

@testable import Dhow

/// Pause All then Resume All gives back what was (UI-GUIDELINES §3).
@MainActor
@Suite struct PauseAllTests {
    func until(_ what: String, _ ok: () throws -> Bool) async throws {
        for _ in 0..<500 where try !ok() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(try ok(), "\(what)")
    }

    @Test func pauseAllThenResumeAllRestoresQueuesAndRuns() async throws {
        let model = AppModel()
        let core = try TempEngine(listener: model.listener)
        let engine = core.engine
        try await model.attach(engine)
        let defaultQueue = try engine.listQueues()[0].id
        let night = try engine.createQueue(
            spec: QueueSpec(name: "night", schedule: nil, onComplete: .nothing))
        _ = try engine.startQueue(id: night.id)
        _ = try engine.stopQueue(
            id: try engine.createQueue(
                spec: QueueSpec(name: "off", schedule: nil, onComplete: .nothing)
            ).id)

        // Nothing listens on port 1: added blind, it keeps reconnecting,
        // `Active` all along (ENGINE-SPEC §9).
        func add(_ start: StartMode) async throws -> String {
            try await engine.add(
                request: AddRequest(
                    url: "http://127.0.0.1:1/\(start)", directory: core.dir.path, start: start,
                    blind: true)
            ).id
        }
        let running = try await add(.queued)
        let paused = try await add(.paused)
        try await until("running") { model.store.downloads[running]?.state.isRunning == true }
        try await until("queues seen") { model.store.queues.count == 3 }

        await model.pauseAll()
        let stopped = try engine.listQueues()
        #expect(stopped.allSatisfy { $0.runState == .stopped })
        // `stop_queue` returns as the stop starts.
        try await until("stopped") { try engine.get(id: running).state == .queued }
        #expect(try engine.get(id: paused).state == .paused)
        #expect(model.canResumeAll)
        try await until("stopped seen") { model.store.downloads[running]?.state == .queued }

        await model.resumeAll()
        let states = Dictionary(
            try engine.listQueues().map { ($0.name ?? "", $0.runState) },
            uniquingKeysWith: { a, _ in a })
        #expect(states == ["": .auto, "night": .started, "off": .stopped])
        try await until("running again") { try engine.get(id: running).state.isRunning }
        #expect(try engine.get(id: paused).state == .paused)
        #expect(model.pausedAll == nil)
        #expect(defaultQueue == stopped.first { $0.isDefault }?.id)
        await core.close()
    }
}
