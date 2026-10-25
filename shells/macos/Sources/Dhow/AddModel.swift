import DhowCore
import Foundation
import Observation

/// The Add dialog's state (UI-GUIDELINES §4): one item per link, each
/// probed before the user confirms. The probe goes through the core, as
/// all HTTP does (§10).
@MainActor @Observable
final class AddModel {
    struct Item: Identifiable {
        enum Probe {
            case running
            case done(ProbeResult)
            case failed(Error)
        }

        var url: String
        var probe = Probe.running
        /// What the user typed over the probed name; `nil`: the core names it.
        var filename: String?

        var id: String { url }

        var name: String {
            if let filename { return filename }
            if case .done(let result) = probe { return result.suggestedName }
            return ""
        }

        var isProbing: Bool {
            if case .running = probe { return true }
            return false
        }

        var failure: Error? {
            if case .failed(let error) = probe { return error }
            return nil
        }
    }

    enum Start: Hashable, CaseIterable {
        case now, queued, paused

        var mode: StartMode {
            switch self {
            case .now: .now
            case .queued: .queued
            case .paused: .paused
            }
        }
    }

    var text: String {
        didSet { syncItems() }
    }
    private(set) var items: [Item] = []
    var directory: URL {
        // Names differ by file system (§3): ask again for the new one.
        didSet {
            if Self.targetFs(for: oldValue) != targetFs {
                reprobe()
            }
        }
    }
    var queue: String
    var start = Start.queued
    /// `nil`: the settings' default (§4.1).
    var connections: Int?
    /// kB/s (1000 bytes, as `Format` shows them); `nil`: no limit of its own.
    var speedLimitKB: Int?
    /// "Name: value" per line.
    var headers = ""
    private(set) var adding = false
    var error: String?

    init(text: String, directory: URL, queue: String) {
        self.text = text
        self.directory = directory
        self.queue = queue
        syncItems()
    }

    /// The http(s) links in pasted text, in order, once each.
    nonisolated static func links(in text: String) -> [String] {
        var seen = Set<String>()
        let words = text.split(whereSeparator: \.isWhitespace).map { trimmed(String($0)) }
        return words.filter { s in
            guard let url = URL(string: s), let scheme = url.scheme?.lowercased(),
                scheme == "http" || scheme == "https", url.host() != nil
            else { return false }
            return seen.insert(s).inserted
        }
    }

    /// A link as written in prose: "(https://x/a_(b).iso)," is
    /// "https://x/a_(b).iso". Leading "(" and "<" go; at the end, the
    /// sentence's punctuation, and ")" or "]" only when unmatched in it.
    nonisolated static func trimmed(_ word: String) -> String {
        var s = Substring(word)
        while let first = s.first, "(<\"'".contains(first) {
            s = s.dropFirst()
        }
        while let last = s.last {
            if ",.;:!?\"'>".contains(last) {
                s = s.dropLast()
            } else if last == ")" || last == "]" {
                let open: Character = last == ")" ? "(" : "["
                guard s.count(where: { $0 == last }) > s.count(where: { $0 == open }) else { break }
                s = s.dropLast()
            } else {
                break
            }
        }
        return String(s)
    }

    /// Lines of "Name: value"; `nil` if one isn't (the core checks the
    /// rest).
    nonisolated static func parseHeaders(_ text: String) -> [Header]? {
        var headers: [Header] = []
        for line in text.split(whereSeparator: \.isNewline) {
            let line = line.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { continue }
            guard let colon = line.firstIndex(of: ":"), colon != line.startIndex else {
                return nil
            }
            headers.append(
                Header(
                    name: line[..<colon].trimmingCharacters(in: .whitespaces),
                    value: line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)))
        }
        return headers
    }

    var headerList: [Header]? { Self.parseHeaders(headers) }

    /// Items follow the text; ones already probed keep their result.
    private func syncItems() {
        let old = Dictionary(items.map { ($0.url, $0) }, uniquingKeysWith: { a, _ in a })
        items = Self.links(in: text).map { old[$0] ?? Item(url: $0) }
    }

    /// The key the view re-probes on: the links, the headers, the folder.
    var probeKey: [String] { items.map(\.url) + [headers, directory.path] }

    /// Names as the destination's file system allows them (ENGINE-SPEC §3):
    /// FAT, exFAT, NTFS and SMB shares take Windows rules.
    var targetFs: TargetFs? { Self.targetFs(for: directory) }

    nonisolated static func targetFs(for directory: URL) -> TargetFs? {
        var info = statfs()
        guard statfs(directory.path, &info) == 0 else { return nil }
        let type = withUnsafeBytes(of: info.f_fstypename) { raw in
            String(decoding: raw.prefix { $0 != 0 }, as: UTF8.self)
        }
        return targetFs(fsType: type)
    }

    nonisolated static func targetFs(fsType: String) -> TargetFs? {
        ["msdos", "exfat", "ntfs", "smbfs"].contains(fsType.lowercased()) ? .windows : nil
    }

    /// Probes the items not yet probed, all at once.
    func probe(with engine: Engine) async {
        guard let headers = headerList else { return }
        let urls = items.filter(\.isProbing).map(\.url)
        let fs = targetFs
        await withTaskGroup(of: (String, Item.Probe).self) { group in
            for url in urls {
                group.addTask {
                    do {
                        let result = try await engine.probe(
                            url: url, headers: headers, targetFs: fs)
                        return (url, .done(result))
                    } catch {
                        return (url, .failed(error))
                    }
                }
            }
            for await (url, result) in group {
                // Superseded (the text changed): the next probe asks again.
                // Otherwise the text may have dropped it.
                if Task.isCancelled { continue }
                if let i = items.firstIndex(where: { $0.url == url }) {
                    items[i].probe = result
                }
            }
        }
    }

    /// Headers changed: every item asks again.
    func reprobe() {
        for i in items.indices {
            items[i].probe = .running
        }
    }

    func rename(_ id: String, to name: String) {
        guard let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].filename = name
    }

    var probing: Bool { items.contains(where: \.isProbing) }

    /// Confirming waits for every probe (§4); a failed one is added blind
    /// if that can help.
    var canConfirm: Bool {
        !items.isEmpty && !probing && !adding && headerList != nil
            && items.allSatisfy { $0.failure.map(ErrorText.blindCanHelp) ?? true }
    }

    /// Some probe failed: the button says "Download anyway".
    var isBlind: Bool { items.contains { $0.failure != nil } }

    func request(for item: Item) -> AddRequest {
        AddRequest(
            url: item.url, headers: headerList ?? [], directory: directory.path,
            filename: item.filename.flatMap { $0.isEmpty ? nil : $0 }, start: start.mode,
            connections: connections.map { UInt8(clamping: $0) }, queue: queue,
            speedLimit: speedLimitKB.map { UInt64($0) * 1000 }, targetFs: targetFs,
            blind: item.failure != nil)
    }

    /// Adds every item; `true` when all went in. A failed one stays listed
    /// with the reason.
    func confirm(with engine: Engine) async -> Bool {
        guard canConfirm else { return false }
        adding = true
        defer { adding = false }
        error = nil
        var failed: [Item] = []
        for item in items {
            do {
                _ = try await engine.add(request: request(for: item))
            } catch {
                self.error = ErrorText.message(error)
                failed.append(item)
            }
        }
        // What went in leaves the dialog; what didn't can be fixed.
        items = failed
        text = failed.map(\.url).joined(separator: "\n")
        return failed.isEmpty
    }
}
