import Foundation
import IOKit.pwr_mgt

/// The power API keep-awake needs, so tests count without asserting.
protocol PowerAssertions: Sendable {
    /// An assertion id, or `nil` if the system refused.
    func preventIdleSleep(reason: String) -> UInt32?
    func release(_ id: UInt32)
}

struct IOKitPower: PowerAssertions {
    func preventIdleSleep(reason: String) -> UInt32? {
        var id = IOPMAssertionID(0)
        let result = IOPMAssertionCreateWithName(
            kIOPMAssertPreventUserIdleSystemSleep as CFString,
            IOPMAssertionLevel(kIOPMAssertionLevelOn), reason as CFString, &id)
        return result == kIOReturnSuccess ? id : nil
    }

    func release(_ id: UInt32) {
        IOPMAssertionRelease(id)
    }
}

/// Keeps the Mac from idle sleep while transfers run (ARCHITECTURE §4):
/// one assertion while any `on` is outstanding, released at the last
/// `off`. The kernel drops it if Dhow crashes; quitting stops every run,
/// which ends it, and `deinit` releases what's left.
final class KeepAwake: @unchecked Sendable {
    private let power: PowerAssertions
    private let reason: String
    private let lock = NSLock()
    private var count = 0
    private var assertion: UInt32?

    init(power: PowerAssertions = IOKitPower(), reason: String = tr("power.reason")) {
        self.power = power
        self.reason = reason
    }

    var isHeld: Bool { lock.withLock { assertion != nil } }

    func set(_ on: Bool) {
        lock.withLock {
            count = on ? count + 1 : max(0, count - 1)
            if count > 0, assertion == nil {
                assertion = power.preventIdleSleep(reason: reason)
            } else if count == 0, let id = assertion {
                power.release(id)
                assertion = nil
            }
        }
    }

    deinit {
        if let assertion {
            power.release(assertion)
        }
    }
}
