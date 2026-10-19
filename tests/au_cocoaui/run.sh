#!/bin/bash
# Compile the production static bridge in actual bundles, plus byte-identical
# sibling-format images without AU classes. Rust callbacks are explicit fixtures.
set -euo pipefail

if [ "$#" -ne 1 ] || [[ "$1" != /* ]] || [ -e "$1" ]; then
    echo "Usage: $0 /absolute/path/to/new-output-directory" >&2
    exit 2
fi
AU_TEST_OUT="$1"
AU_TEST_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
mkdir -p "$AU_TEST_OUT/ImageIsolation.app/Contents/MacOS"

for AU_TEST_BUNDLE in First.component Second.component VST3.vst3 CLAP.clap; do
    mkdir -p "$AU_TEST_OUT/$AU_TEST_BUNDLE/Contents/MacOS"
    cat > "$AU_TEST_OUT/$AU_TEST_BUNDLE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundleIdentifier</key><string>io.intrect.test.au.$AU_TEST_BUNDLE</string>
<key>CFBundlePackageType</key><string>BNDL</string>
</dict></plist>
PLIST
done
for AU_TEST_NAME in First Second; do
    xcrun clang -fobjc-arc -fmodules -Wall -Wextra -Werror \
        -mmacosx-version-min=11.0 -dynamiclib \
        "-DNIH_PLUG_AU_VIEW_CLASS=NihPlugAuViewFactory_Test$AU_TEST_NAME" \
        "-DNIH_PLUG_AU_CONTAINER_CLASS=NihPlugAuContainerView_Test$AU_TEST_NAME" \
        "$AU_TEST_ROOT/src/wrapper/au/cocoaui.m" \
        "$AU_TEST_ROOT/tests/au_cocoaui/bridge_fixture.m" \
        -framework AppKit -framework AudioToolbox \
        -o "$AU_TEST_OUT/$AU_TEST_NAME.component/Contents/MacOS/Fixture"
done
xcrun clang -fobjc-arc -fmodules -Wall -Wextra -Werror \
    -mmacosx-version-min=11.0 -dynamiclib -DNIH_PLUG_AU_COCOAUI_DISABLED=1 \
    "$AU_TEST_ROOT/src/wrapper/au/cocoaui.m" \
    "$AU_TEST_ROOT/tests/au_cocoaui/bridge_fixture.m" \
    -framework AppKit -framework AudioToolbox \
    -o "$AU_TEST_OUT/VST3.vst3/Contents/MacOS/Fixture"
ditto "$AU_TEST_OUT/VST3.vst3/Contents/MacOS/Fixture" "$AU_TEST_OUT/CLAP.clap/Contents/MacOS/Fixture"
cmp "$AU_TEST_OUT/VST3.vst3/Contents/MacOS/Fixture" "$AU_TEST_OUT/CLAP.clap/Contents/MacOS/Fixture"
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
for AU_TEST_MODE in siblings-first au-first; do
    AU_TEST_RESULT="$AU_TEST_OUT/$AU_TEST_MODE.json"
    AU_TEST_LAUNCH=(open -n -W "$AU_TEST_OUT/ImageIsolation.app" --args "$AU_TEST_MODE"
        "$AU_TEST_OUT/First.component/Contents/MacOS/Fixture"
        "$AU_TEST_OUT/Second.component/Contents/MacOS/Fixture"
        "$AU_TEST_OUT/VST3.vst3/Contents/MacOS/Fixture"
        "$AU_TEST_OUT/CLAP.clap/Contents/MacOS/Fixture" "$AU_TEST_RESULT")
    if [ "$(launchctl managername)" != Aqua ]; then
        AU_TEST_LAUNCH=(sudo -n launchctl asuser "$(id -u)" sudo -n -u "$(id -un)" "${AU_TEST_LAUNCH[@]}")
    fi
    "${AU_TEST_LAUNCH[@]}"
    # open's exit status alone does not prove that the native assertions passed.
    test -s "$AU_TEST_RESULT"
    cat "$AU_TEST_RESULT"
done
