#!/bin/bash
# AUv2 instrument의 실제 AudioComponent 등록, MusicDevice 입력, audio render,
# MIDI output callback을 한 번에 검증한다. macOS 26의 bare-CLI 탐색 회귀를
# 피하기 위해 MiniHost를 .app으로 감싸 LaunchServices로 실행한다.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_DIR="$ROOT_DIR/target/au_midi_smoke"
HOST_SOURCE="$ROOT_DIR/scripts/au_midi_smoke/MiniHost.c"
HOST_PLIST="$ROOT_DIR/scripts/au_midi_smoke/Info.plist"
HOST_BINARY="$BUILD_DIR/MiniHost"
HOST_APP="$BUILD_DIR/MiniHost.app"
RESULT_JSON="$BUILD_DIR/result.json"
AU_SOURCE="$ROOT_DIR/target/bundled/Sine Test Tone.component"
AU_DESTINATION="$HOME/Library/Audio/Plug-Ins/Components/Sine Test Tone.component"
PREVIOUS_AU="$BUILD_DIR/previous/Sine Test Tone.component"
INSTALLED_AU="$BUILD_DIR/installed/Sine Test Tone.component"

mkdir -p "$BUILD_DIR" "$HOME/Library/Audio/Plug-Ins/Components"

if [ -e "$PREVIOUS_AU" ]; then
    echo "ERROR: 이전 실행의 AU 백업이 남아 있습니다: $PREVIOUS_AU" >&2
    exit 1
fi

restore_component() {
    if [ -e "$AU_DESTINATION" ]; then
        mkdir -p "$(dirname "$INSTALLED_AU")"
        if [ -e "$INSTALLED_AU" ]; then
            INSTALLED_AU="$BUILD_DIR/installed/Sine Test Tone-$(date +%s).component"
        fi
        mv "$AU_DESTINATION" "$INSTALLED_AU"
    fi
    if [ -e "$PREVIOUS_AU" ]; then
        mv "$PREVIOUS_AU" "$AU_DESTINATION"
    fi
    killall -9 AudioComponentRegistrar 2>/dev/null || true
}
trap restore_component EXIT

if [ -e "$AU_DESTINATION" ]; then
    mkdir -p "$(dirname "$PREVIOUS_AU")"
    mv "$AU_DESTINATION" "$PREVIOUS_AU"
fi

cd "$ROOT_DIR"
cargo xtask bundle sine --release
test -d "$AU_SOURCE"
ditto "$AU_SOURCE" "$AU_DESTINATION"
codesign --verify --deep --strict "$AU_DESTINATION"

xcrun clang -O2 -Wall -Wextra -Werror "$HOST_SOURCE" \
    -framework AudioToolbox -framework CoreMIDI -framework CoreFoundation \
    -o "$HOST_BINARY"

mkdir -p "$HOST_APP/Contents/MacOS"
cp "$HOST_BINARY" "$HOST_APP/Contents/MacOS/MiniHost"
cp "$HOST_PLIST" "$HOST_APP/Contents/Info.plist"
codesign --force --sign - "$HOST_APP"

killall -9 AudioComponentRegistrar 2>/dev/null || true
: > "$RESULT_JSON"
open -W -n "$HOST_APP" --args --out "$RESULT_JSON"

test -f "$RESULT_JSON"
cat "$RESULT_JSON"
if grep -q '"error"' "$RESULT_JSON" ||
   ! grep -q '"audio_ok":true' "$RESULT_JSON" ||
   ! grep -q '"note_on_echo":true' "$RESULT_JSON" ||
   ! grep -q '"note_off_echo":true' "$RESULT_JSON" ||
   ! grep -q '"last_render_error":0' "$RESULT_JSON" ||
   ! grep -q '"tail_time_infinite":true' "$RESULT_JSON"; then
    exit 1
fi
