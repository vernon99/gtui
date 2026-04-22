#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_PATH="$ROOT/src-tauri/target/debug/bundle/macos/GTUI.app"
BIN_PATH="$APP_PATH/Contents/MacOS/gtui"

if pgrep -f "$BIN_PATH" >/dev/null 2>&1; then
  pkill -f "$BIN_PATH"
  sleep 1
fi

(cd "$ROOT/src-tauri" && cargo tauri build --debug)

open -n "$APP_PATH"
