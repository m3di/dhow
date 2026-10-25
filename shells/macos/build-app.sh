#!/usr/bin/env bash
# Assembles Dhow.app from the SwiftPM build, as Xcode would: the
# executable, its resource bundle (the strings) in Contents/Resources, an
# Info.plist, the icon, then an ad-hoc signature (DECISIONS 2026-10-25 "M4
# scope": no Developer ID yet). Notifications and the Dock need a real
# bundle, which `swift run` isn't.
#
#   build-app.sh           release build
#   build-app.sh --debug   debug build, quicker
#
# Writes shells/macos/build/Dhow.app. Needs the DhowCore package built
# first: core/dhow-ffi/build-swift.sh (or --debug).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
cd "$here"

config=release
case "${1:-}" in
    "") ;;
    --debug) config=debug ;;
    *) echo "usage: $0 [--debug]" >&2; exit 2 ;;
esac

if [ ! -d ../apple/DhowCore/DhowCoreFFI.xcframework ]; then
    echo "DhowCore isn't built: run core/dhow-ffi/build-swift.sh first" >&2
    exit 1
fi

swift build -c "$config" --product Dhow
bin="$(swift build -c "$config" --show-bin-path)"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/core/Cargo.toml" | head -1)"

out="$here/build"
app="$out/Dhow.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/Dhow" "$app/Contents/MacOS/Dhow"
cp -R "$bin/Dhow_Dhow.bundle" "$app/Contents/Resources/"
# What macOS reads from the app's own bundle: the Services menu's title.
cp -R Bundle/*.lproj "$app/Contents/Resources/"

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
