#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export GTUI_OPEN_DEVTOOLS=1

exec "$ROOT/scripts/rebuild_and_run.sh"
