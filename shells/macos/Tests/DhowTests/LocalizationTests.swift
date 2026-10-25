import Foundation
import Testing

@testable import Dhow

/// Every user-visible string goes through a key (DECISIONS 2026-10-25 "M4
/// scope"): each `tr(...)` in the sources has an entry, and no SwiftUI view
/// is given a literal.
@Suite struct LocalizationTests {
    static let sources = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("Sources/Dhow")

    static func swiftFiles() throws -> [(String, String)] {
        let files = FileManager.default.enumerator(at: sources, includingPropertiesForKeys: nil)!
        return try files.compactMap { $0 as? URL }.filter { $0.pathExtension == "swift" }.map {
            ($0.lastPathComponent, try String(contentsOf: $0, encoding: .utf8))
        }
    }

    /// Keys with the number of arguments each takes.
    static func keys() throws -> [String: Int] {
        let lproj = sources.appendingPathComponent("Resources/en.lproj")
        let strings = try #require(
            NSDictionary(contentsOf: lproj.appendingPathComponent("Localizable.strings"))
                as? [String: String])
        let plurals = try #require(
            NSDictionary(contentsOf: lproj.appendingPathComponent("Localizable.stringsdict"))
                as? [String: Any])
        let specifier = try Regex("%(@|lld)")
        var keys: [String: Int] = [:]
        for key in Array(strings.keys) + Array(plurals.keys) {
            keys[key] = key.matches(of: specifier).count
        }
        return keys
    }

    @Test func everyKeyUsedIsDefined() throws {
        let keys = try Self.keys()
        let call = try Regex(#"tr\("((?:[^"\\]|\\.)*)"\)"#)
        var used = 0
        for (file, text) in try Self.swiftFiles() {
            for match in text.matches(of: call) {
                let literal = String(match.output[1].substring!)
                let args = literal.components(separatedBy: "\\(").count - 1
                let name = literal.prefix { $0 != " " && $0 != "\\" }
                let defined = keys.first { $0.key.split(separator: " ").first == name }
                #expect(defined != nil, "\(file): no entry for \(name)")
                #expect(defined?.value == args, "\(file): \(name) takes \(args) arguments")
                used += 1
            }
        }
        #expect(used > 50)
    }

    @Test func viewsAreNeverGivenLiterals() throws {
        let literal = try Regex(
            #"\b(Text|Button|Label|Toggle|Section|TableColumn|Picker|TextField|Menu|Stepper|LabeledContent|DisclosureGroup|Window|help|navigationTitle|alert|confirmationDialog|accessibilityLabel|accessibilityValue|badge)\("[^"]"#
        )
        for (file, text) in try Self.swiftFiles() {
            #expect(text.firstMatch(of: literal) == nil, "\(file) shows a literal string")
        }
    }

    @Test func stringsResolveFromTheBundle() {
        #expect(tr("app.name") == "Dhow")
        #expect(tr("status.waitingFor \(1)") == "Waiting for 1 download to finish")
        #expect(tr("status.waitingFor \(2)") == "Waiting for 2 downloads to finish")
        #expect(tr("group.count \(2) \(3)") == "(2 of 3)")
    }

    /// What `tr` asserts on in debug builds: an argument of the wrong type
    /// misses its entry and comes back as the key.
    @Test func aMismatchedArgumentReadsAsAKey() {
        #expect(looksLikeKey(String(localized: "fail.http \(String(404))", bundle: .dhow)))
        #expect(!looksLikeKey(tr("fail.http \(404)")))
        #expect(!looksLikeKey("4.1 MB/s"))
        #expect(!looksLikeKey("Dhow"))
    }
}
