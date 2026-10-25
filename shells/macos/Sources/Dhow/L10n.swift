import Foundation

extension Bundle {
    /// The app's resources. In Dhow.app, `build-app.sh` puts SwiftPM's
    /// resource bundle in Contents/Resources, where `Bundle.module` (which
    /// looks beside the .app's Contents) doesn't; `swift run` and tests use
    /// `Bundle.module`.
    static let dhow: Bundle = {
        if let url = Bundle.main.resourceURL?.appendingPathComponent("Dhow_Dhow.bundle"),
            let bundle = Bundle(url: url)
        {
            return bundle
        }
        return .module
    }()
}

/// Every user-visible string: a key in `en.lproj/Localizable.strings` (or
/// `.stringsdict` for plurals). English only for now; translations come as
/// packs over the same keys (DECISIONS 2026-10-25 "M4 scope").
func tr(_ key: String.LocalizationValue) -> String {
    let text = String(localized: key, bundle: .dhow)
    #if DEBUG
        // A key with no entry comes back as itself, as does one whose
        // arguments don't match its entry's (`\(String(n))` for `%lld`).
        assert(!looksLikeKey(text), "no localized string for \(text)")
    #endif
    return text
}

/// "status.left 3 min": a lowercase word, a dot, then a letter.
func looksLikeKey(_ text: String) -> Bool {
    let head = text.prefix { $0 != " " }
    guard let dot = head.firstIndex(of: "."), dot != head.startIndex else { return false }
    let after = head.index(after: dot)
    return head[..<dot].allSatisfy { $0.isLowercase && $0.isLetter }
        && after < head.endIndex && head[after].isLetter
}
