// swift-tools-version:5.9
// dhow-core for the macOS (and later iOS) shells. `core/dhow-ffi/build-swift.sh`
// generates Sources/DhowCore/DhowCore.swift and DhowCoreFFI.xcframework;
// neither is committed. Builds with `swift build` on Command Line Tools.
import PackageDescription

let package = Package(
    name: "DhowCore",
    platforms: [.macOS(.v14), .iOS(.v17)],
    products: [
        .library(name: "DhowCore", targets: ["DhowCore"])
    ],
    targets: [
        .binaryTarget(name: "DhowCoreFFI", path: "DhowCoreFFI.xcframework"),
        .target(
            name: "DhowCore",
            dependencies: ["DhowCoreFFI"],
            // rustls' platform verifier.
            linkerSettings: [.linkedFramework("Security")]
        ),
        // `swift run dhow-core-smoke`: the bindings work across the boundary.
        .executableTarget(name: "dhow-core-smoke", dependencies: ["DhowCore"], path: "Sources/DhowCoreSmoke"),
    ]
)
