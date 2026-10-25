// `swift run dhow-core-smoke` (build-swift.sh runs it): the generated Swift
// links against dhow-ffi and works across the boundary both ways (calls,
// async calls, errors, the event listener, the platform adapter). Not a
// test suite: the Rust tests in core/dhow-ffi/tests cover behaviour (XCTest
// needs Xcode).

import DhowCore
import Foundation

final class Listener: EventListener, @unchecked Sendable {
    let settingsChanged = DispatchSemaphore(value: 0)

    func onEvent(event: Event) {
        if case .settingsChanged = event {
            settingsChanged.signal()
        }
    }

    func waitSettingsChanged() -> Bool {
        settingsChanged.wait(timeout: .now() + 5) == .success
    }
}

final class Adapter: PlatformAdapter, @unchecked Sendable {
    func fileTokenCreate(path: String) throws -> Data? { nil }
    func fileTokenResolve(token: Data) throws -> ResolvedFile? { throw AdapterError.Unsupported }
    func systemProxy(forUrl: String) throws -> ProxyConfig { ProxyConfig() }
    func networkInfo() throws -> AdapterNetworkInfo {
        AdapterNetworkInfo(interfaceKind: .wifi, vpnActive: true, systemProxy: nil)
    }
    func keepAwake(on: Bool) {}
    func backgroundCapability() -> BackgroundCapability { .unrestricted }
}

func check(_ ok: Bool, _ what: String) {
    if !ok {
        print("smoke: FAILED: \(what)")
        exit(1)
    }
}

let data = FileManager.default.temporaryDirectory
    .appendingPathComponent("dhow-smoke-\(UUID().uuidString)")
defer { try? FileManager.default.removeItem(at: data) }
let listener = Listener()
let engine = try Engine.open(
    dataDir: data.path, config: EngineConfig(), listener: listener, adapter: Adapter())

check(!coreVersion().isEmpty, "version")
check(try engine.settings().proxyMode == .system, "default proxy mode")
var patch = SettingsPatch()
patch.globalSpeedLimit = .rate(bytesPerSec: 1 << 20)
patch.proxyMode = .direct
let settings = try engine.setSettings(patch: patch)
check(settings.globalSpeedLimit == 1 << 20 && settings.proxyMode == .direct, "patch")
check(listener.waitSettingsChanged(), "SettingsChanged event")

let net = try engine.networkInfo()
check(net.interfaceKind == .wifi && net.vpnActive && net.proxyInUse == .direct, "adapter")

let night = ScheduleWindow(
    days: [.monday, .friday], start: TimeOfDay(hour: 22, minute: 0),
    end: TimeOfDay(hour: 6, minute: 30))
let queue = try engine.createQueue(
    spec: QueueSpec(
        name: "night", schedule: Schedule(windows: [night], action: .start),
        onComplete: .nothing))
check(try engine.listQueues().count == 2 && queue.schedule?.windows == [night], "queues")
check(Category.allCases.count == 9, "CaseIterable")

let dir = FileManager.default.temporaryDirectory.path
do {
    _ = try await engine.add(
        request: AddRequest(url: "not a url", directory: dir, start: .now))
    check(false, "bad URL accepted")
} catch DhowError.InvalidArgument {
}
do {
    // Nothing listens on port 1.
    _ = try await engine.add(
        request: AddRequest(url: "http://127.0.0.1:1/x", directory: dir, start: .queued))
    check(false, "unreachable server accepted")
} catch DhowError.Probe(let reason) {
    check(reason == .connect, "probe failure \(reason)")
}
check(try engine.list().isEmpty, "nothing added")
await engine.shutdown()
print("smoke: ok (DhowCore \(coreVersion()))")
