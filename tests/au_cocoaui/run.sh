#!/bin/bash
# Compile the production bridge, load two byte-identical copies, and exercise
# cached-factory/instance lifetimes. Rust callbacks are explicit fixtures.
set -euo pipefail

if [ "$#" -ne 1 ] || [[ "$1" != /* ]] || [ -e "$1" ]; then
    echo "Usage: $0 /absolute/path/to/new-output-directory" >&2
    exit 2
fi
AU_TEST_OUT="$1"
AU_TEST_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$AU_TEST_OUT/ImageIsolation.app/Contents/MacOS"

xcrun clang -fobjc-arc -fmodules -Wall -Wextra -Werror \
    -mmacosx-version-min=11.0 -dynamiclib \
    "$AU_TEST_ROOT/src/wrapper/au/cocoaui.m" \
    "$AU_TEST_ROOT/tests/au_cocoaui/bridge_fixture.m" \
    -framework AppKit -framework AudioToolbox -o "$AU_TEST_OUT/first.dylib"
ditto "$AU_TEST_OUT/first.dylib" "$AU_TEST_OUT/second.dylib"
cmp "$AU_TEST_OUT/first.dylib" "$AU_TEST_OUT/second.dylib"
xcrun clang -fobjc-arc -fmodules -Wall -Wextra -Werror \
    -mmacosx-version-min=11.0 "$AU_TEST_ROOT/tests/au_cocoaui/image_isolation.m" \
    -framework AppKit -framework AudioToolbox \
    -o "$AU_TEST_OUT/ImageIsolation.app/Contents/MacOS/image_isolation"
cat > "$AU_TEST_OUT/ImageIsolation.app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>image_isolation</string>
<key>CFBundleIdentifier</key><string>io.intrect.test.nih-plug.au-image-isolation</string>
<key>CFBundleName</key><string>ImageIsolation</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$AU_TEST_OUT/ImageIsolation.app"
AU_TEST_LAUNCH=(open -n -W "$AU_TEST_OUT/ImageIsolation.app" --args
    "$AU_TEST_OUT/first.dylib" "$AU_TEST_OUT/second.dylib" "$AU_TEST_OUT/result.json")
if [ "$(launchctl managername)" != Aqua ]; then
    AU_TEST_LAUNCH=(sudo -n launchctl asuser "$(id -u)" sudo -n -u "$(id -un)" "${AU_TEST_LAUNCH[@]}")
fi
"${AU_TEST_LAUNCH[@]}"
# open's exit status alone does not prove that the native assertions passed.
test -s "$AU_TEST_OUT/result.json"
cat "$AU_TEST_OUT/result.json"
