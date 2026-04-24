#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/logs/gtui-window.png}"

mkdir -p "$(dirname "$OUT")"

WINDOW_ID="$(
  swift -e '
import CoreGraphics
import Foundation

let windows = CGWindowListCopyWindowInfo(.optionAll, kCGNullWindowID)! as! [[String: Any]]
let matches = windows.compactMap { window -> Int? in
    let owner = window[kCGWindowOwnerName as String] as? String ?? ""
    let title = window[kCGWindowName as String] as? String ?? ""
    let layer = window[kCGWindowLayer as String] as? Int ?? -1
    let bounds = window[kCGWindowBounds as String] as? [String: Any] ?? [:]
    let width = bounds["Width"] as? Int ?? 0
    let height = bounds["Height"] as? Int ?? 0
    guard owner == "GTUI", title == "Gas Town Dashboard", layer == 0, width > 800, height > 500 else {
        return nil
    }
    return window[kCGWindowNumber as String] as? Int
}

if let id = matches.first {
    print(id)
}
'
)"

if [[ -z "$WINDOW_ID" ]]; then
  echo "Could not find the GTUI dashboard window id." >&2
  exit 1
fi

screencapture -x -l "$WINDOW_ID" "$OUT"
echo "$OUT"
