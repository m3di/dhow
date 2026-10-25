#!/usr/bin/env bash
# Assembles Dhow.app from the SwiftPM build, as Xcode would: the
# executable, its resource bundle (the strings) in Contents/Resources, an
# Info.plist, the icon, then an ad-hoc signature (DECISIONS 2026-10-25 "M4
# scope": no Developer ID yet). Notifications and the Dock need a real
# bundle, which `swift run` isn't.
#
#   build-app.sh              release build, this Mac's architecture
#   build-app.sh --universal  release build for arm64 and x86_64 (the DMG's,
#                             needs build-swift.sh's universal DhowCore)
#   build-app.sh --debug      debug build, quicker
#
# The license and the third-party credits (CREDITS.txt, from cargo
# metadata) go in Contents/Resources: the DMG must point to UniFFI's
# source (ARCHITECTURE §9).
#
# Writes shells/macos/build/Dhow.app. Needs the DhowCore package built
# first: core/dhow-ffi/build-swift.sh (or --debug).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cd "$here"

config=release
universal=false
case "${1:-}" in
    "") ;;
    --debug) config=debug ;;
    --universal) universal=true ;;
    *) echo "usage: $0 [--debug | --universal]" >&2; exit 2 ;;
esac

if [ ! -d ../apple/DhowCore/DhowCoreFFI.xcframework ]; then
    echo "DhowCore isn't built: run core/dhow-ffi/build-swift.sh first" >&2
    exit 1
fi

if $universal; then
    # One build per architecture (Command Line Tools have no multi-arch
    # SwiftPM build), then one executable.
    bins=()
    for arch in arm64 x86_64; do
        triple="$arch-apple-macosx14.0"
        swift build -c release --product Dhow --triple "$triple"
        bins+=("$(swift build -c release --triple "$triple" --show-bin-path)")
    done
    bin="${bins[0]}"
    mkdir -p "$here/build"
    lipo -create "${bins[0]}/Dhow" "${bins[1]}/Dhow" -output "$here/build/Dhow-universal"
    exe="$here/build/Dhow-universal"
else
    swift build -c "$config" --product Dhow
    bin="$(swift build -c "$config" --show-bin-path)"
    exe="$bin/Dhow"
fi
# The version's one source: the workspace's Cargo.toml.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/core/Cargo.toml" | head -1)"

out="$here/build"
app="$out/Dhow.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$exe" "$app/Contents/MacOS/Dhow"
cp -R "$bin/Dhow_Dhow.bundle" "$app/Contents/Resources/"
# What macOS reads from the app's own bundle: the Services menu's title.
cp -R Bundle/*.lproj "$app/Contents/Resources/"
cp "$root/LICENSE" "$app/Contents/Resources/LICENSE"
python3 "$here/credits.py" "$root/core" > "$app/Contents/Resources/CREDITS.txt"

# The icon, redrawn only when its script changes.
icns="$out/Dhow.icns"
if [ ! -f "$icns" ] || [ Icon/make-icon.swift -nt "$icns" ]; then
    set="$out/Dhow.iconset"
    rm -rf "$set"
    mkdir -p "$set"
    swift Icon/make-icon.swift "$out/icon-1024.png"
    for s in 16 32 128 256 512; do
        sips -z "$s" "$s" "$out/icon-1024.png" --out "$set/icon_${s}x${s}.png" >/dev/null
        d=$((s * 2))
        sips -z "$d" "$d" "$out/icon-1024.png" --out "$set/icon_${s}x${s}@2x.png" >/dev/null
    done
    iconutil -c icns "$set" -o "$icns"
fi
cp "$icns" "$app/Contents/Resources/Dhow.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>app.dhow.Dhow</string>
	<key>CFBundleName</key>
	<string>Dhow</string>
	<key>CFBundleDisplayName</key>
	<string>Dhow</string>
	<key>CFBundleExecutable</key>
	<string>Dhow</string>
	<key>CFBundleIconFile</key>
	<string>Dhow</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleLocalizations</key>
	<array>
		<string>en</string>
	</array>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSPrincipalClass</key>
	<string>NSApplication</string>
	<key>NSHumanReadableCopyright</key>
	<string>GPL-3.0</string>
	<key>NSAppleEventsUsageDescription</key>
	<string>Dhow asks macOS to shut down when a queue you set to do so has finished.</string>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>Download with Dhow</string>
			</dict>
			<key>NSMessage</key>
			<string>downloadURLs</string>
			<key>NSPortName</key>
			<string>Dhow</string>
			<key>NSSendTypes</key>
			<array>
				<string>public.url</string>
				<string>public.utf8-plain-text</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
EOF

codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict "$app"
echo "Built $app ($config, $version)"
