#!/bin/bash
# Exercise the actual bundler/build.rs with the existing Gain AU example.
# No plugin installation or product editor/audio acceptance is performed.
set -euo pipefail
if [ "$#" -ne 1 ] || [[ "$1" != /* ]] || [ -e "$1" ]; then
    echo "Usage: $0 /absolute/path/to/new-output-directory" >&2
    exit 2
fi
AU_BUNDLE_OUT="$1"
AU_BUNDLE_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
AU_BUNDLE_TARGET="${CARGO_TARGET_DIR:-$AU_BUNDLE_ROOT/target}"
if [[ "$AU_BUNDLE_TARGET" != /* ]]; then
    AU_BUNDLE_TARGET="$AU_BUNDLE_ROOT/$AU_BUNDLE_TARGET"
fi
mkdir -p "$AU_BUNDLE_OUT"
cd "$AU_BUNDLE_ROOT"
CARGO_TARGET_DIR="$AU_BUNDLE_TARGET" cargo xtask bundle gain --release --features au --locked
xcrun clang -fobjc-arc -fmodules -Wall -Wextra -Werror \
    -mmacosx-version-min=11.0 "$AU_BUNDLE_ROOT/tests/au_cocoaui/bundle_resolution.m" \
    -framework Foundation -framework AudioToolbox -o "$AU_BUNDLE_OUT/BundleResolution"
for AU_BUNDLE_MODE in siblings-first au-first; do
    "$AU_BUNDLE_OUT/BundleResolution" "$AU_BUNDLE_MODE" \
        "$AU_BUNDLE_TARGET/bundled/Gain.component/Contents/MacOS/Gain" \
        "$AU_BUNDLE_TARGET/bundled/Gain.vst3/Contents/MacOS/Gain" \
        "$AU_BUNDLE_TARGET/bundled/Gain.clap/Contents/MacOS/Gain" \
        > "$AU_BUNDLE_OUT/$AU_BUNDLE_MODE.log"
    cat "$AU_BUNDLE_OUT/$AU_BUNDLE_MODE.log"
done
