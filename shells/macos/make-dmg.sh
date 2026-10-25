#!/usr/bin/env bash
# Builds the Dhow DMG on the maintainer's Mac (ROADMAP M4.5, DECISIONS
# 2026-10-25 "M4 scope"): a clean universal release of DhowCore and
# Dhow.app, a DMG with the app, an /Applications link, the license and
# the credits, then checks it. Ad-hoc signed unless told otherwise; no CI.
#
#   make-dmg.sh                                   ad-hoc signed DMG
#   make-dmg.sh --sign "Developer ID Application: Name (TEAMID)"
#                                                 hardened runtime, signed
#   make-dmg.sh --sign "…" --notarize <profile>   also notarized and stapled
#                                                 (`xcrun notarytool
#                                                 store-credentials <profile>`)
#   make-dmg.sh --clean                           rebuild Rust from scratch
#   make-dmg.sh --no-launch                       skip the launch check
#
# Writes shells/macos/build/Dhow-<version>.dmg. The Rust release has its
# own target dir (core/target/dmg), built only with this script's
# environment, so no object built for another deployment target or
# profile is linked; --clean deletes it first.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
core="$root/core"
out="$here/build"

identity=""
profile=""
clean=false
launch=true
while [ $# -gt 0 ]; do
    case "$1" in
        --sign) identity="${2:?--sign needs an identity}"; shift 2 ;;
        --notarize) profile="${2:?--notarize needs a keychain profile}"; shift 2 ;;
        --clean) clean=true; shift ;;
        --no-launch) launch=false; shift ;;
        *) echo "usage: $0 [--sign <identity> [--notarize <profile>]] [--clean] [--no-launch]" >&2
           exit 2 ;;
    esac
done

fail() { echo "make-dmg: $*" >&2; exit 1; }
step() { echo "==> $*"; }

[ -n "$profile" ] && [ -z "$identity" ] && fail "--notarize needs --sign (Apple notarizes Developer ID signatures only)"
for tool in cargo swift lipo hdiutil codesign spctl python3; do
    command -v "$tool" >/dev/null || fail "$tool not found"
done
if [ -n "$identity" ]; then
    security find-identity -v -p codesigning | grep -qF "$identity" \
        || fail "no signing identity \"$identity\" in the keychain"
fi
if [ -n "$profile" ]; then
    xcrun --find notarytool >/dev/null 2>&1 || fail "notarytool not found (install Xcode or recent Command Line Tools)"
    xcrun --find stapler >/dev/null 2>&1 || fail "stapler not found (install Xcode or recent Command Line Tools)"
fi

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$core/Cargo.toml" | head -1)"
[ -n "$version" ] || fail "no version in core/Cargo.toml"
dmg="$out/Dhow-$version.dmg"

step "DhowCore, universal release"
export CARGO_TARGET_DIR="$core/target/dmg"
$clean && rm -rf "$CARGO_TARGET_DIR"
"$core/dhow-ffi/build-swift.sh"

step "Dhow.app, universal release"
(cd "$here" && swift package clean)
"$here/build-app.sh" --universal
app="$out/Dhow.app"
lipo "$app/Contents/MacOS/Dhow" -verify_arch arm64 x86_64 || fail "the app isn't universal"
plutil -lint "$app/Contents/Info.plist" >/dev/null
[ "$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$app/Contents/Info.plist")" = "$version" ] \
    || fail "Info.plist doesn't carry version $version"

if [ -n "$identity" ]; then
    step "Signing with $identity (hardened runtime)"
    codesign --force --options runtime --timestamp \
        --entitlements "$here/Dhow.entitlements" --sign "$identity" "$app"
fi
codesign --verify --deep --strict --verbose=2 "$app"

step "Disk image"
stage="$out/dmg"
rm -rf "$stage" "$dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
cp "$root/LICENSE" "$stage/LICENSE.txt"
cp "$app/Contents/Resources/CREDITS.txt" "$stage/CREDITS.txt"
hdiutil create -volname "Dhow $version" -srcfolder "$stage" -fs HFS+ -format UDZO \
    -ov "$dmg" >/dev/null
rm -rf "$stage"
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
fi

if [ -n "$profile" ]; then
    step "Notarizing (this waits for Apple)"
    xcrun notarytool submit "$dmg" --keychain-profile "$profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
fi

step "Verifying"
hdiutil verify "$dmg"
# Physical paths: LaunchServices runs the app as /private/var/…, not the
# /var/… symlink `mktemp` gives, and the checks below match on its path.
mnt="$(cd "$(mktemp -d "${TMPDIR:-/tmp}/dhow-dmg.XXXXXX")" && pwd -P)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/dhow-dmg-data.XXXXXX")"
cleanup() {
    pkill -TERM -f "$mnt/Dhow.app/Contents/MacOS/Dhow" 2>/dev/null || true
    hdiutil detach "$mnt" -quiet 2>/dev/null || hdiutil detach "$mnt" -force -quiet 2>/dev/null || true
    rm -rf "$scratch"
    rmdir "$mnt" 2>/dev/null || true
}
trap cleanup EXIT
hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mnt" -quiet
for f in Dhow.app Applications LICENSE.txt CREDITS.txt; do
    [ -e "$mnt/$f" ] || fail "$f missing from the DMG"
done
codesign --verify --deep --strict "$mnt/Dhow.app"
grep -q "mozilla/uniffi-rs" "$mnt/CREDITS.txt" || fail "CREDITS.txt doesn't point to UniFFI's source"

if $launch; then
    # From the mounted volume, with its own data and log directories, so
    # nothing of the user's is read or written (and nothing is downloaded).
    open -n --env DHOW_DATA_DIR="$scratch/data" --env DHOW_LOG_DIR="$scratch/logs" "$mnt/Dhow.app"
    pid=""
    for _ in $(seq 1 150); do
        pid="$(pgrep -f "$mnt/Dhow.app/Contents/MacOS/Dhow" || true)"
        [ -n "$pid" ] && [ -e "$scratch/data/dhow.sqlite" ] && break
        sleep 0.2
    done
    [ -n "$pid" ] || fail "the app didn't start from the DMG"
    [ -e "$scratch/data/dhow.sqlite" ] || fail "the app started but didn't open its engine"
    kill -TERM "$pid"
    for _ in $(seq 1 150); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    kill -0 "$pid" 2>/dev/null && fail "the app didn't quit on SIGTERM"
    echo "Launched from the DMG and quit cleanly."
fi

# Gatekeeper: an ad-hoc signature is rejected (expected: open it with
# right-click > Open, or remove the quarantine attribute); a notarized
# Developer ID app is accepted.
if spctl -a -vv -t exec "$mnt/Dhow.app" 2>&1; then
    echo "Gatekeeper accepts it."
elif [ -n "$profile" ]; then
    fail "Gatekeeper rejects the notarized app"
else
    echo "Gatekeeper rejects it, as expected for an ad-hoc or unnotarized signature."
fi

echo "Built $dmg"
