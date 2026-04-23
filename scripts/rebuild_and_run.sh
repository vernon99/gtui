#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_BUNDLE_PATH="$ROOT/src-tauri/target/debug/bundle/macos/GTUI.app"
APP_BIN_PATH="$ROOT/src-tauri/target/debug/bundle/macos/GTUI.app/Contents/MacOS/gtui"
RAW_BIN_PATH="$ROOT/src-tauri/target/debug/gtui"
LOG_PATH="$ROOT/logs/gtui-run.log"
PID_PATH="$ROOT/logs/gtui.pid"
export GT_ROOT="${GT_ROOT:-$HOME/gt}"
FORCE_BUNDLE_REFRESH="${GTUI_REFRESH_BUNDLE:-}"

wait_for_exit() {
  local pattern="$1"
  for _ in {1..20}; do
    if ! pgrep -f "$pattern" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

pkill -f "$APP_BIN_PATH" >/dev/null 2>&1 || true
pkill -f "$RAW_BIN_PATH" >/dev/null 2>&1 || true
wait_for_exit "$APP_BIN_PATH" || true
wait_for_exit "$RAW_BIN_PATH" || true

mkdir -p "$ROOT/logs"

needs_bundle_refresh=0
if [[ -n "$FORCE_BUNDLE_REFRESH" || ! -x "$APP_BIN_PATH" || ! -f "$APP_BUNDLE_PATH/Contents/Info.plist" ]]; then
  needs_bundle_refresh=1
fi

if [[ "$needs_bundle_refresh" -eq 1 ]]; then
  echo "Rebuilding app bundle shell"
  (cd "$ROOT/src-tauri" && cargo tauri build --debug)
else
  echo "Rebuilding Rust binary only"
  (cd "$ROOT/src-tauri" && cargo build --bin gtui)
  install -m 755 "$RAW_BIN_PATH" "$APP_BIN_PATH"
fi

echo "Launching $RAW_BIN_PATH"
echo "App: $APP_BUNDLE_PATH"
echo "GT_ROOT: $GT_ROOT"
echo "Logs: $LOG_PATH"
open_args=(
  -n
  "$APP_BUNDLE_PATH"
  --stdout "$LOG_PATH"
  --stderr "$LOG_PATH"
  --env "GT_ROOT=$GT_ROOT"
)

if [[ -n "${GTUI_OPEN_DEVTOOLS:-}" ]]; then
  open_args+=(--env "GTUI_OPEN_DEVTOOLS=$GTUI_OPEN_DEVTOOLS")
fi

open "${open_args[@]}"

APP_PID=""
for _ in {1..20}; do
  APP_PID="$(pgrep -f "$APP_BIN_PATH" | head -n 1 || true)"
  if [[ -n "$APP_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$APP_PID" ]]; then
  echo "$APP_PID" >"$PID_PATH"
  echo "PID: $APP_PID"
else
  rm -f "$PID_PATH"
  echo "PID: not found"
fi
