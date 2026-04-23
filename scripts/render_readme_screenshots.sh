#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIXTURE="$ROOT/frontend/screenshots/readme.html"
OUT_DIR="$ROOT/docs/assets"
SWIFT="${SWIFT:-swift}"

if ! command -v "$SWIFT" >/dev/null 2>&1; then
  echo "swift is required to render README screenshots with macOS WebKit." >&2
  exit 1
fi

if [[ ! -f "$FIXTURE" ]]; then
  echo "Missing screenshot fixture: $FIXTURE" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"

"$SWIFT" "$ROOT/scripts/render_readme_screenshots.swift" \
  "$FIXTURE" \
  "$OUT_DIR/task-spine.png" \
  "$OUT_DIR/mayor-chat.png"

for image in "$OUT_DIR/task-spine.png" "$OUT_DIR/mayor-chat.png"; do
  width="$(sips -g pixelWidth "$image" 2>/dev/null | awk '/pixelWidth/ {print $2}')"
  height="$(sips -g pixelHeight "$image" 2>/dev/null | awk '/pixelHeight/ {print $2}')"
  if [[ "$width" != "1400" || "$height" != "860" ]]; then
    sips -z 860 1400 "$image" >/dev/null
  fi
done

echo "Rendered README screenshots:"
echo "  $OUT_DIR/task-spine.png"
echo "  $OUT_DIR/mayor-chat.png"
