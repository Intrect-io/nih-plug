#!/bin/bash
# 추가 AU property 조회 없이 지연 알림이 전달되는지 실제 AU factory로 확인한다.
# 설치된 플러그인/registrar를 수정하지 않고 매 실행의 증거를 보존한다.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BUILD_ROOT="$ROOT_DIR/target/au_notification_smoke"
mkdir -p "$BUILD_ROOT"
RUN_DIR="$(mktemp -d "$BUILD_ROOT/run-XXXXXX")"
export CARGO_TARGET_DIR="$ROOT_DIR/target"

cd "$ROOT_DIR"
cargo run --locked --example au_property_notification_lifetime --features au \
    > "$RUN_DIR/lifetime.log" 2>&1
cat "$RUN_DIR/lifetime.log"
cargo build --locked --manifest-path scripts/au_notification_smoke/fixture/Cargo.toml \
    > "$RUN_DIR/fixture-build.log" 2>&1

FIXTURE="$CARGO_TARGET_DIR/debug/libau_notification_fixture.dylib"
test -f "$FIXTURE"
codesign --force --sign - "$FIXTURE"
codesign --verify --strict "$FIXTURE"

HOST_APP="$RUN_DIR/LatencyNotifyHost.app"
mkdir -p "$HOST_APP/Contents/MacOS"
swiftc -O scripts/au_notification_smoke/LatencyNotifyHost.swift \
    -o "$HOST_APP/Contents/MacOS/LatencyNotifyHost"
cat > "$HOST_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>LatencyNotifyHost</string>
<key>CFBundleIdentifier</key><string>io.intrect.nih-plug.notification-smoke</string>
<key>CFBundleName</key><string>LatencyNotifyHost</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
codesign --force --sign - "$HOST_APP"
shasum -a 256 "$FIXTURE" "$HOST_APP/Contents/MacOS/LatencyNotifyHost" \
    > "$RUN_DIR/images.sha256"

RESULT_JSON="$RUN_DIR/result.json"
open -W -n "$HOST_APP" --args "$RESULT_JSON" "$FIXTURE"
test -f "$RESULT_JSON"
cat "$RESULT_JSON"
test "$(plutil -extract status raw -o - "$RESULT_JSON")" = "PASS"
