#!/usr/bin/env bash
# Builds the `DhowCore` Swift package in shells/apple/DhowCore (ARCHITECTURE
# §9): dhow-ffi as a static library, the Swift sources UniFFI generates from
# it, and DhowCoreFFI.xcframework holding the library and its C header. Then
# checks the package with `swift build` and runs its smoke program.
#
#   build-swift.sh          release, macOS arm64 + x86_64 (one universal
#                           archive). Command Line Tools are enough: the
#                           XCFramework (macOS slice only) is laid out by hand.
#   build-swift.sh --debug  this Mac's architecture, dev profile: quicker
#                           after its first run, for iterating.
#   build-swift.sh --ios    also iOS and the iOS simulator (arm64); the
#                           XCFramework then comes from
#                           `xcodebuild -create-xcframework`, so needs Xcode.
#
# Needs the rustup targets aarch64-apple-darwin and x86_64-apple-darwin
# (plus aarch64-apple-ios and aarch64-apple-ios-sim for --ios). Generated
# files are not committed (see shells/apple/DhowCore/.gitignore).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
core="$(dirname "$here")"
root="$(dirname "$core")"
pkg="$root/shells/apple/DhowCore"
build="$core/target/swift"
xcf="$pkg/DhowCoreFFI.xcframework"
debug_target="$core/target/swift-debug"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
# UI-GUIDELINES §1: macOS 14, iOS 17 (for the C code: SQLite, aws-lc).
export MACOSX_DEPLOYMENT_TARGET=14.0 IPHONEOS_DEPLOYMENT_TARGET=17.0

mode="${1:-release}"
case "$mode" in
    release | --release) mode=release ;;
    --debug) mode=debug ;;
    --ios) mode=ios ;;
    *) echo "usage: $0 [--debug | --ios]" >&2; exit 2 ;;
esac

# Only here is dhow-ffi built as a static archive, so `cargo test` never
# links one.
staticlib() { # <target> | ""  (host, dev profile)
    if [ -z "$1" ]; then
        # Its own target dir: C objects `cargo test` built without the
        # deployment target would be linked as they are (cc doesn't rebuild
        # for it).
        (cd "$core" && CARGO_TARGET_DIR="$debug_target" \
            cargo rustc -p dhow-ffi --lib --crate-type staticlib)
    else
        (cd "$core" && cargo rustc -p dhow-ffi --lib --crate-type staticlib --target "$1" --release)
    fi
}
# `CARGO_TARGET_DIR` (make-dmg.sh gives the release its own) or the default.
rel() { echo "${CARGO_TARGET_DIR:-$core/target}/$1/release/libdhow_ffi.a"; }

# Swift sources, and the C header and module map in $build/gen/include. In
# the workspace: the generator reads uniffi.toml through `cargo metadata`.
generate() { # <library>
    rm -rf "$build/gen"
    (cd "$core" && cargo run -p dhow-ffi --features bindgen --bin uniffi-bindgen -- \
        generate --library "$1" --language swift --out-dir "$build/gen")
    mkdir -p "$build/gen/include"
    mv "$build/gen/DhowCoreFFI.h" "$build/gen/include/"
    mv "$build/gen/DhowCoreFFI.modulemap" "$build/gen/include/module.modulemap"
}

# What `xcodebuild -create-xcframework` writes for one macOS static
# library, which SwiftPM reads without Xcode.
macos_xcframework() { # <library> <arch>...
    local lib="$1" id="macos-$(IFS=_; echo "${*:2}")" archs=""
    for a in "${@:2}"; do archs+="<string>$a</string>"; done
    rm -rf "$xcf"
    mkdir -p "$xcf/$id"
    cp "$lib" "$xcf/$id/libdhow_ffi.a"
    cp -R "$build/gen/include" "$xcf/$id/Headers"
    cat > "$xcf/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AvailableLibraries</key>
	<array>
		<dict>
			<key>BinaryPath</key>
			<string>libdhow_ffi.a</string>
			<key>HeadersPath</key>
			<string>Headers</string>
			<key>LibraryIdentifier</key>
			<string>$id</string>
			<key>LibraryPath</key>
			<string>libdhow_ffi.a</string>
			<key>SupportedArchitectures</key>
			<array>$archs</array>
			<key>SupportedPlatform</key>
			<string>macos</string>
		</dict>
	</array>
	<key>CFBundlePackageType</key>
	<string>XFWK</string>
	<key>XCFrameworkFormatVersion</key>
	<string>1.0</string>
</dict>
</plist>
EOF
}

case "$mode" in
    debug)
        staticlib ""
        generate "$debug_target/debug/libdhow_ffi.a"
        macos_xcframework "$debug_target/debug/libdhow_ffi.a" "$(uname -m)"
        ;;
    release | ios)
        staticlib aarch64-apple-darwin
        staticlib x86_64-apple-darwin
        generate "$(rel aarch64-apple-darwin)"
        mkdir -p "$build/macos"
        lipo -create "$(rel aarch64-apple-darwin)" "$(rel x86_64-apple-darwin)" \
            -output "$build/macos/libdhow_ffi.a"
        if [ "$mode" = release ]; then
            macos_xcframework "$build/macos/libdhow_ffi.a" arm64 x86_64
        else
            staticlib aarch64-apple-ios
            staticlib aarch64-apple-ios-sim
            rm -rf "$xcf"
            xcodebuild -create-xcframework \
                -library "$build/macos/libdhow_ffi.a" -headers "$build/gen/include" \
                -library "$(rel aarch64-apple-ios)" -headers "$build/gen/include" \
                -library "$(rel aarch64-apple-ios-sim)" -headers "$build/gen/include" \
                -output "$xcf"
        fi
        ;;
esac

mkdir -p "$pkg/Sources/DhowCore"
cp "$build/gen/DhowCore.swift" "$pkg/Sources/DhowCore/"
# The package builds as a shell would use it, and works across the boundary.
(cd "$pkg" && swift build && swift run dhow-core-smoke)
echo "DhowCore package ready in $pkg"
