#!/usr/bin/env bash
# Side-by-side smoke test: launch legacy Python webui and the Rust backend
# against the same GT_ROOT, fetch /api/snapshot from both, and diff the
# structurally-comparable fields. Prints pass/fail per section.
#
# Usage:  scripts/parity_smoke.sh [GT_ROOT]
# Defaults GT_ROOT to $GT_ROOT → $HOME/gt.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GT_ROOT_ARG="${1:-${GT_ROOT:-$HOME/gt}}"
PORT="${PARITY_PORT:-8431}"

if ! command -v jq >/dev/null 2>&1; then
  echo "parity_smoke: jq is required" >&2
  exit 2
fi

PY_OUT="$(mktemp -t gtui-parity-py.XXXXXX.json)"
RS_OUT="$(mktemp -t gtui-parity-rs.XXXXXX.json)"
WEBUI_LOG="$(mktemp -t gtui-parity-webui.XXXXXX.log)"
cleanup() {
  rm -f "$PY_OUT" "$RS_OUT" "$WEBUI_LOG"
  if [[ -n "${WEBUI_PID:-}" ]] && kill -0 "$WEBUI_PID" 2>/dev/null; then
    kill "$WEBUI_PID" 2>/dev/null || true
    wait "$WEBUI_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Build the Rust snapshot dumper once so timing is comparable.
(cd "$ROOT/src-tauri" && cargo build --example snapshot_dump --quiet)

# Start the Python webui in the background.
python3 "$ROOT/webui/server.py" --gt-root "$GT_ROOT_ARG" --port "$PORT" \
  >"$WEBUI_LOG" 2>&1 &
WEBUI_PID=$!

# Wait for the snapshot to hydrate (store is empty on boot — poller populates
# it after ~1s). Give up after ~20s.
for _ in $(seq 1 40); do
  if curl -sf "http://127.0.0.1:$PORT/api/snapshot" -o "$PY_OUT" 2>/dev/null; then
    nodes=$(jq '.graph.nodes | length' "$PY_OUT" 2>/dev/null || echo 0)
    agents=$(jq '.agents | length' "$PY_OUT" 2>/dev/null || echo 0)
    if [[ "$nodes" -gt 0 || "$agents" -gt 0 ]]; then break; fi
  fi
  sleep 0.5
done
if ! [[ -s "$PY_OUT" ]]; then
  echo "parity_smoke: webui never responded on :$PORT" >&2
  cat "$WEBUI_LOG" >&2
  exit 1
fi

"$ROOT/src-tauri/target/debug/examples/snapshot_dump" "$GT_ROOT_ARG" >"$RS_OUT"

report() {
  local label=$1 expr=$2
  local py rs
  py=$(jq -cS "$expr" "$PY_OUT")
  rs=$(jq -cS "$expr" "$RS_OUT")
  if [[ "$py" == "$rs" ]]; then
    printf '  %-32s match   %s\n' "$label" "$py"
  else
    printf '  %-32s DIFFER\n    py:   %s\n    rust: %s\n' "$label" "$py" "$rs"
  fi
}

echo "GT_ROOT: $GT_ROOT_ARG"
echo "Counts:"
report "graph.nodes"              '.graph.nodes|length'
report "graph.edges"              '.graph.edges|length'
report "activity.groups"          '.activity.groups|length'
report "activity.unassigned"      '.activity.unassigned_agents|length'
report "git.repos"                '.git.repos|length'
report "git.recent_commits"       '.git.recent_commits|length'
report "git.task_memory (keys)"   '.git.task_memory|length'
report "convoys.convoys"          '.convoys.convoys|length'
report "convoys.task_index"       '.convoys.task_index|length'
report "crews"                    '.crews|length'
report "agents"                   '.agents|length'
report "stores"                   '.stores|length'
report "alerts"                   '.alerts|length'
report "status_legend"            '.status_legend|length'
report "errors"                   '.errors|length'

echo "Summary card:"
report "summary"                  '.summary'

echo "Alerts (ordered):"
report "alerts"                   '.alerts'

echo "First 3 agents (core fields):"
report "agents[0..3]"             '[.agents[:3][] | {target, kind, role, scope, label, has_session, session_name, pane_id}]'

echo "Repos:"
report "git.repos ids"            '[.git.repos[] | {id, label, scope, root}]'

echo "First 3 convoys:"
report "convoys[0..3]"            '[.convoys.convoys[:3][] | {id, title, status, total, completed}]'

echo "First 3 stores:"
report "stores[0..3]"             '[.stores[:3][] | {name, scope, total, open, closed, blocked, hooked}]'

echo "Activity groups:"
report "groups"                   '[.activity.groups[] | {task_id, title, stored_status, ui_status, scope, is_system, agent_count}]'

# Terminal + diff probes. Pick a real target/repo/sha from the snapshot.
TARGET=$(jq -r '[.agents[] | select(.has_session) | .target][0] // ""' "$PY_OUT")
if [[ -n "$TARGET" ]]; then
  echo "Terminal ($TARGET):"
  PY_TERM=$(curl -sf "http://127.0.0.1:$PORT/api/terminal?target=$TARGET" \
            | jq -c '{has_claude: (.claude_view // null | . != null),
                      has_codex: (.codex_view // null | . != null),
                      has_transcript: (.transcript_view // null | . != null)}')
  echo "  py:   $PY_TERM"
  echo "  (Rust terminal path is IPC; exercised by src-tauri tests)"
fi

REPO=$(jq -r '.git.repos[0].id // ""' "$PY_OUT")
SHA=$(jq -r '.git.recent_commits[0].sha // ""' "$PY_OUT")
if [[ -n "$REPO" && -n "$SHA" ]]; then
  echo "Git diff ($REPO / ${SHA:0:7}):"
  PY_DIFF=$(curl -sf "http://127.0.0.1:$PORT/api/git/diff?repo=$REPO&sha=$SHA" \
            | jq -c '{truncated, lines: (.text | split("\n") | length)}')
  echo "  py:   $PY_DIFF"
  echo "  (Rust diff path is IPC; exercised by src-tauri tests)"
fi
