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
    String(localized: key, bundle: .dhow)
}
