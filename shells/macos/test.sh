#!/usr/bin/env bash
# `swift test` for the app's package. With Command Line Tools only (no
# Xcode), SwiftPM finds neither the Testing framework nor its interop
# library, so they are passed here; with Xcode selected this is plain
# `swift test`. Extra arguments go to `swift test`.
set -euo pipefail
cd "$(dirname "$0")"

dev="$(xcode-select -p)"
if [[ "$dev" == */CommandLineTools ]]; then
    frameworks="$dev/Library/Developer/Frameworks"
    exec swift test \
        -Xswiftc -F"$frameworks" \
        -Xlinker -F"$frameworks" \
        -Xlinker -rpath -Xlinker "$frameworks" \
        -Xlinker -rpath -Xlinker "$dev/Library/Developer/usr/lib" \
        "$@"
fi
exec swift test "$@"
