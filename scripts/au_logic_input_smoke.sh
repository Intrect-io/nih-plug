#!/bin/bash
# Logic Pro의 AUv2 effect input pull 계약을 재현해 AUD-831 회귀를 막는다.
#
# LogicPullHost는 유효한 sample time을 요구하면서 input callback 뒤 mData 포인터를
# zero-copy source buffer로 바꾼다. 이 두 조건에서 Gain AU의 출력이 non-silent여야
# 한다. AU 설치는 기존 컴포넌트를 임시로 보관하고 항상 복원한다.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_DIR="$ROOT_DIR/target/au_logic_input_smoke"
HOST_SOURCE="$ROOT_DIR/scripts/au_logic_input_smoke/LogicPullHost.swift"
HOST_PLIST="$ROOT_DIR/scripts/au_logic_input_smoke/Info.plist"
HOST_BINARY="$BUILD_DIR/LogicPullHost"
HOST_APP="$BUILD_DIR/LogicPullHost.app"
AU_SOURCE="$ROOT_DIR/target/bundled/Gain.component"
AU_DESTINATION="$HOME/Library/Audio/Plug-Ins/Components/Gain.component"
PREVIOUS_AU="$BUILD_DIR/previous/Gain.component"
INSTALLED_AU="$BUILD_DIR/installed/Gain.component"
RESULT_JSON=""

mkdir -p "$BUILD_DIR" "$HOME/Library/Audio/Plug-Ins/Components"

if [ -e "$PREVIOUS_AU" ]; then
    echo "ERROR: 이전 실행의 AU 백업이 남아 있습니다: $PREVIOUS_AU" >&2
    exit 1
fi

restore_component() {
    if [ -e "$AU_DESTINATION" ]; then
        mkdir -p "$(dirname "$INSTALLED_AU")"
        if [ -e "$INSTALLED_AU" ]; then
            INSTALLED_AU="$BUILD_DIR/installed/Gain-$(date +%s).component"
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
cargo xtask bundle gain --release
test -d "$AU_SOURCE"
ditto "$AU_SOURCE" "$AU_DESTINATION"
codesign --verify --deep --strict "$AU_DESTINATION"

swiftc -O "$HOST_SOURCE" -o "$HOST_BINARY"
mkdir -p "$HOST_APP/Contents/MacOS"
cp "$HOST_BINARY" "$HOST_APP/Contents/MacOS/LogicPullHost"
cp "$HOST_PLIST" "$HOST_APP/Contents/Info.plist"
codesign --force --sign - "$HOST_APP"

killall -9 AudioComponentRegistrar 2>/dev/null || true
for attempt in 1 2 3; do
    candidate="$BUILD_DIR/result-$$-$attempt.json"
    if open -W -n "$HOST_APP" --args \
        --type aufx --subtype MPgN --manufacturer MoiP \
        --sample-rate 44100 --block-size 512 --blocks 32 --out "$candidate"; then
        if [ -f "$candidate" ]; then
            RESULT_JSON="$candidate"
            break
        fi
    fi
    echo "Logic-style AU host attempt $attempt did not produce a result; retrying" >&2
    sleep 1
done

if [ -z "$RESULT_JSON" ]; then
    echo "ERROR: Logic-style AU host produced no result" >&2
    exit 1
fi

cat "$RESULT_JSON"
if ! grep -q '"error":""' "$RESULT_JSON" ||
   ! grep -q '"target_found":true' "$RESULT_JSON" ||
   ! grep -q '"callback_status":0' "$RESULT_JSON" ||
   ! grep -q '"initialize_status":0' "$RESULT_JSON" ||
   ! grep -q '"render_status":0' "$RESULT_JSON" ||
   ! grep -q '"silent":false' "$RESULT_JSON"; then
    exit 1
fi
