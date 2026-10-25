import DhowCore
import Foundation
import Network

/// What the network path says, as the core and the Settings card want it
/// (ARCHITECTURE §8).
struct NetworkFacts: Equatable, Sendable {
    var kind: InterfaceKind
    var vpn: Bool
    /// Metered (a phone's hotspot, cellular).
    var expensive: Bool
    /// Low Data Mode.
    var constrained: Bool

    /// An interface the path uses, best first.
    struct Interface: Equatable {
        enum Kind { case wifi, ethernet, cellular, loopback, other }
        var kind: Kind
        var name: String
    }

    /// A path update after `previous`: the facts, and whether they differ
    /// (the engine is told only then). No usable path is an unknown one.
    static func next(
        after previous: NetworkFacts?, satisfied: Bool, interfaces: [Interface], expensive: Bool,
        constrained: Bool
    ) -> (facts: NetworkFacts, changed: Bool) {
        let now =
            satisfied
            ? from(interfaces: interfaces, expensive: expensive, constrained: constrained)
            : NetworkFacts(kind: .unknown, vpn: false, expensive: false, constrained: false)
        return (now, now != previous)
    }

    /// The default route's interface decides the kind; a tunnel (`utun`,
    /// `ipsec`, `ppp`, `tun`, `tap`) among the path's interfaces is a VPN.
    static func from(interfaces: [Interface], expensive: Bool, constrained: Bool) -> NetworkFacts {
        func isTunnel(_ i: Interface) -> Bool {
            i.kind == .other
                && ["utun", "ipsec", "ppp", "tun", "tap"].contains { i.name.hasPrefix($0) }
        }
        let first = interfaces.first { $0.kind != .loopback }
        let kind: InterfaceKind =
            switch first {
            case let i? where isTunnel(i): .vpn
            case let i? where i.kind == .wifi: .wifi
            case let i? where i.kind == .ethernet: .ethernet
            case let i? where i.kind == .cellular: .cellular
            default: .unknown
            }
        return NetworkFacts(
            kind: kind, vpn: interfaces.contains(where: isTunnel), expensive: expensive,
            constrained: constrained)
    }
}

/// `NWPathMonitor`: event-driven, nothing polls. Each change is kept for
/// `network_info` and told to the engine (`network_changed`, which emits
/// `NetworkChanged`), on the monitor's own queue, never inside an adapter
/// call. Holds the engine weakly.
final class NetworkMonitor: @unchecked Sendable {
    private let monitor = NWPathMonitor()
    private let queue = DispatchQueue(label: "app.dhow.network")
    private let lock = NSLock()
    private var facts: NetworkFacts?
    weak var engine: Engine?
    /// Each change, on the main queue, for the Settings card.
    var onChange: (@MainActor (NetworkFacts) -> Void)?

    /// The last path seen; `nil` before the first.
    var current: NetworkFacts? {
        lock.withLock { facts }
    }

    func start() {
        monitor.pathUpdateHandler = { [weak self] path in
            self?.update(path)
        }
        monitor.start(queue: queue)
    }

    func stop() {
        monitor.cancel()
    }

    private func update(_ path: NWPath) {
        let interfaces = path.availableInterfaces.map { i in
            NetworkFacts.Interface(kind: Self.kind(i.type), name: i.name)
        }
        let (now, changed) = lock.withLock {
            let next = NetworkFacts.next(
                after: facts, satisfied: path.status == .satisfied, interfaces: interfaces,
                expensive: path.isExpensive, constrained: path.isConstrained)
            facts = next.facts
            return next
        }
        guard changed else { return }
        _ = try? engine?.networkChanged()
        if let onChange {
            DispatchQueue.main.async { onChange(now) }
        }
    }

    private static func kind(_ t: NWInterface.InterfaceType) -> NetworkFacts.Interface.Kind {
        switch t {
        case .wifi: .wifi
        case .wiredEthernet: .ethernet
        case .cellular: .cellular
        case .loopback: .loopback
        default: .other
        }
    }
}
