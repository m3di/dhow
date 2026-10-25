// swift-tools-version:6.0
// The macOS app (UI-GUIDELINES §8): a SwiftPM package, so Command Line Tools
// build it and Xcode opens it unchanged. `build-app.sh` assembles Dhow.app.
// Needs the DhowCore package built first (core/dhow-ffi/build-swift.sh).
import PackageDescription

let package = Package(
    name: "Dhow",
    defaultLocalization: "en",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../apple/DhowCore")
    ],
    targets: [
        .executableTarget(
            name: "Dhow",
            dependencies: [.product(name: "DhowCore", package: "DhowCore")],
            resources: [.process("Resources")]
        ),
        .testTarget(name: "DhowTests", dependencies: ["Dhow"]),
    ]
)
