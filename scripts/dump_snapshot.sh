#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export GT_ROOT="${GT_ROOT:-$HOME/gt}"

cargo run --quiet --manifest-path "$ROOT/src-tauri/Cargo.toml" --example dump_snapshot -- "$@"
