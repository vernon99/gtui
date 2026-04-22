#!/usr/bin/env python3
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import os
import re
import shlex
import subprocess
import threading
import time
import zlib
from dataclasses import dataclass
from datetime import datetime
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, unquote, urlparse


APP_ROOT = Path(__file__).resolve().parent
DEFAULT_GT_ROOT = (Path.home() / "gt").resolve()
DEFAULT_LOCAL_BIN = Path.home() / ".local" / "bin"
CODEX_SESSIONS_ROOT = Path.home() / ".codex" / "sessions"
CODEX_ROLLOUT_SCAN_LIMIT = 160
CODEX_ROLLOUT_LIST_TTL_SECONDS = 3.0
CODEX_ROLLOUT_HEAD_BYTES = 32768
CLAUDE_PROJECTS_ROOT = Path.home() / ".claude" / "projects"
CLAUDE_SESSION_SCAN_LIMIT = 120
CLAUDE_SESSION_LIST_TTL_SECONDS = 3.0
CLAUDE_SESSION_HEAD_BYTES = 32768

JSON_HEADERS = {
    "Content-Type": "application/json; charset=utf-8",
    "Cache-Control": "no-store",
}
HTML_HEADERS = {
    "Content-Type": "text/html; charset=utf-8",
    "Cache-Control": "no-store",
}


def ensure_local_bin_on_path() -> None:
    local_bin = str(DEFAULT_LOCAL_BIN)
    path_parts = os.environ.get("PATH", "").split(os.pathsep)
    if local_bin not in path_parts:
        os.environ["PATH"] = os.pathsep.join([local_bin, *path_parts])
TEXT_HEADERS = {
    "Content-Type": "text/plain; charset=utf-8",
    "Cache-Control": "no-store",
}
STATIC_ROOT = APP_ROOT / "static"
STATIC_CONTENT_TYPES = {
    ".css": "text/css; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".mjs": "text/javascript; charset=utf-8",
    ".json": "application/json; charset=utf-8",
    ".svg": "image/svg+xml",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
}

SECTION_RE = re.compile(r"^[-\u2500]+\s+(.+?)\s+[-\u2500]+$")
EVENT_RE = re.compile(
    r"^\[(?P<time>[^\]]+)\]\s+(?P<symbol>\S+)\s+(?P<actor>.+?)\s{2,}(?P<message>.+)$"
)
SLUNG_EVENT_RE = re.compile(r"^slung\s+(?P<task>\S+)\s+to\s+(?P<target>\S+)$")
DONE_EVENT_RE = re.compile(r"^done:\s+(?P<task>\S+)$")
ISSUE_ID_RE = re.compile(r"\b(?:hq|[a-z]{2,})-[a-z0-9]+(?:\.[a-z0-9]+)*\b", re.IGNORECASE)
GRAPH_ALLOWED_TYPES = {"task", "bug", "feature", "chore", "decision", "epic"}
STATUS_LEGEND = [
    {"name": "open", "icon": "○", "category": "active", "meaning": "Available to work (default)"},
    {"name": "in_progress", "icon": "◐", "category": "wip", "meaning": "Actively being worked on"},
    {"name": "blocked", "icon": "●", "category": "wip", "meaning": "Blocked by a dependency"},
    {"name": "deferred", "icon": "❄", "category": "frozen", "meaning": "Deliberately put on ice for later"},
    {"name": "closed", "icon": "✓", "category": "done", "meaning": "Completed"},
    {"name": "pinned", "icon": "📌", "category": "frozen", "meaning": "Persistent, stays open indefinitely"},
    {"name": "hooked", "icon": "◇", "category": "wip", "meaning": "Attached to an agent hook"},
]


def now_iso() -> str:
    return datetime.now().astimezone().isoformat(timespec="seconds")


def display_command(args: list[str]) -> str:
    return " ".join(shlex.quote(part) for part in args)


def deep_copy_json(value: Any) -> Any:
    return json.loads(json.dumps(value))


def parse_json_loose(text: str) -> Any:
    if not text:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def normalize_lines(text: str, limit: int = 16) -> list[str]:
    lines = [line.rstrip() for line in text.splitlines()]
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    if len(lines) > limit:
        lines = lines[-limit:]
    return lines


def clip_text(text: str, limit: int = 160) -> str:
    compact = re.sub(r"\s+", " ", str(text or "").strip())
    if len(compact) <= limit:
        return compact
    return compact[: max(0, limit - 3)].rstrip() + "..."


def normalize_path_value(path_text: str) -> str:
    if not path_text:
        return ""
    try:
        return os.path.normpath(os.path.realpath(path_text))
    except OSError:
        return os.path.normpath(path_text)


def normalize_command_name(command: str) -> str:
    return os.path.basename(str(command or "").strip()).lower()


def is_codex_command(command: str) -> bool:
    return normalize_command_name(command) == "codex"


def is_claude_command(command: str) -> bool:
    # Claude Code may appear as node in tmux because the CLI is a Node process.
    return normalize_command_name(command) in {"claude", "claude.exe", "node"}


def read_file_head(path: Path, max_bytes: int) -> str:
    try:
        with path.open("rb") as handle:
            data = handle.read(max_bytes)
    except OSError:
        return ""
    return data.decode("utf-8", errors="replace")


def read_file_text(path: Path) -> str:
    try:
        data = path.read_bytes()
    except OSError:
        return ""
    return data.decode("utf-8", errors="replace")


def iter_jsonl_records(text: str) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(payload, dict):
            records.append(payload)
    return records


def extract_message_text(content: Any) -> str:
    if not isinstance(content, list):
        return ""
    chunks: list[str] = []
    for part in content:
        if not isinstance(part, dict):
            continue
        part_type = str(part.get("type") or "")
        if part_type not in {"input_text", "output_text"}:
            continue
        text = str(part.get("text") or "").strip()
        if text:
            chunks.append(text)
    return "\n\n".join(chunks).strip()


def is_hidden_transcript_message(text: str) -> bool:
    normalized = str(text or "").strip()
    if not normalized:
        return False
    if normalized.startswith("<turn_aborted>") and normalized.endswith("</turn_aborted>"):
        return True
    if normalized.startswith("# AGENTS.md instructions for "):
        return True
    return False


def stringify_tool_output(output: Any) -> str:
    if output is None:
        return ""
    if isinstance(output, str):
        return output
    try:
        return json.dumps(output, indent=2, sort_keys=True)
    except TypeError:
        return str(output)


def excerpt_tool_output(text: str, max_lines: int = 18, max_chars: int = 3200) -> str:
    lines = [line.rstrip() for line in str(text or "").splitlines()]
    while lines and not lines[0].strip():
        lines.pop(0)
    while lines and not lines[-1].strip():
        lines.pop()
    if len(lines) > max_lines:
        head = lines[:8]
        tail = lines[-8:]
        lines = head + ["..."] + tail
    excerpt = "\n".join(lines)
    if len(excerpt) <= max_chars:
        return excerpt
    return excerpt[: max(0, max_chars - 4)].rstrip() + "\n..."


def summarize_tool_output(text: str) -> str:
    lines = [line.strip() for line in str(text or "").splitlines() if line.strip()]
    if not lines:
        return "Tool returned no output."
    command_line = next((line for line in lines if line.startswith("Command:")), "")
    process_line = next((line for line in lines if line.startswith("Process ")), "")
    error_line = next((line for line in lines if line.lower().startswith("error")), "")
    output_line = next((line for line in lines if line.startswith("Output:")), "")
    if error_line:
        return clip_text(error_line, 140)
    if command_line and process_line:
        return clip_text(f"{command_line} | {process_line}", 180)
    if process_line:
        return clip_text(process_line, 140)
    if command_line:
        return clip_text(command_line, 160)
    if output_line:
        return clip_text(output_line, 140)
    return clip_text(lines[0], 160)


def summarize_tool_call(name: str, arguments: Any) -> str:
    parsed: Any = arguments
    if isinstance(arguments, str):
        parsed = parse_json_loose(arguments)
        if parsed is None:
            parsed = arguments

    if name == "exec_command" and isinstance(parsed, dict):
        command_text = str(parsed.get("cmd") or "").strip()
        workdir = str(parsed.get("workdir") or "").strip()
        if command_text and workdir:
            return clip_text(f"{command_text} @ {workdir}", 180)
        if command_text:
            return clip_text(command_text, 180)

    if name.lower() == "bash" and isinstance(parsed, dict):
        command_text = str(parsed.get("command") or "").strip()
        description = str(parsed.get("description") or "").strip()
        if command_text and description:
            return clip_text(f"{description}: {command_text}", 180)
        if command_text:
            return clip_text(command_text, 180)

    if isinstance(parsed, dict):
        for key in ("message", "cmd", "command", "description", "file_path", "path", "pattern", "query", "question", "target", "chars"):
            value = str(parsed.get(key) or "").strip()
            if value:
                return clip_text(f"{name}: {value}", 180)
    return clip_text(name or "tool call", 180)


def summarize_codex_event(payload: dict[str, Any]) -> str:
    event_type = str(payload.get("type") or "")
    if event_type == "task_started":
        return "Turn started"
    if event_type == "task_complete":
        return "Turn completed"
    if event_type == "context_compacted":
        return "Context compacted"
    if event_type == "turn_aborted":
        return "Turn aborted"
    message = str(payload.get("message") or "").strip()
    if message:
        return clip_text(message, 180)
    return clip_text(event_type.replace("_", " ") or "event", 180)


def encode_claude_project_dir(path_text: str) -> str:
    normalized = normalize_path_value(path_text)
    return normalized.replace(os.sep, "-") if normalized else ""


def stringify_claude_tool_output(tool_result: Any, fallback: Any = "") -> str:
    if isinstance(tool_result, dict):
        stdout = str(tool_result.get("stdout") or "")
        stderr = str(tool_result.get("stderr") or "")
        if stdout or stderr:
            return "\n".join(part for part in (stdout, stderr) if part).strip()
    if fallback:
        return stringify_tool_output(fallback)
    return stringify_tool_output(tool_result)


def extract_claude_text_block(block: dict[str, Any]) -> str:
    text = block.get("text")
    if isinstance(text, str):
        return text.strip()
    content = block.get("content")
    if isinstance(content, str):
        return content.strip()
    return ""


def summarize_claude_event(record: dict[str, Any]) -> str:
    subtype = str(record.get("subtype") or "")
    if subtype == "stop_hook_summary":
        return "Stop hook summary"
    if subtype == "turn_duration":
        count = record.get("messageCount")
        return f"Turn duration · {count} messages" if count is not None else "Turn duration"
    event_type = str(record.get("type") or "")
    return clip_text((subtype or event_type).replace("_", " ") or "event", 180)


def format_timestamp_short(timestamp: str) -> str:
    if not timestamp:
        return ""
    try:
        dt = datetime.fromisoformat(timestamp.replace("Z", "+00:00")).astimezone()
    except ValueError:
        return timestamp
    return dt.strftime("%H:%M:%S")


def match_path_score(target_path: str, session_cwd: str) -> int:
    target = normalize_path_value(target_path)
    session = normalize_path_value(session_cwd)
    if not target or not session:
        return -1
    if target == session:
        return 4000 + len(session)
    if session.startswith(target + os.sep):
        return 3000 + len(session)
    if target.startswith(session + os.sep):
        return 2000 + len(session)
    return -1


def worker_count(total: int, cap: int = 8) -> int:
    if total <= 0:
        return 1
    return max(1, min(total, cap))


def parse_simple_metadata_block(text: str) -> dict[str, str]:
    metadata: dict[str, str] = {}
    for line in text.splitlines()[:20]:
        match = re.match(r"^([a-zA-Z0-9_.-]+):\s*(.+)$", line.strip())
        if not match:
            continue
        metadata[match.group(1)] = match.group(2)
    return metadata


def find_issue_ids(text: str) -> list[str]:
    return sorted({match.group(0) for match in ISSUE_ID_RE.finditer(text or "")})


def count_issue_statuses(issues: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for issue in issues:
        status = issue.get("status", "unknown")
        counts[status] = counts.get(status, 0) + 1
    return counts


@dataclass
class CommandResult:
    ok: bool
    args: list[str]
    cwd: str
    duration_ms: int
    data: Any = None
    stdout: str = ""
    stderr: str = ""
    error: str = ""
    returncode: int | None = None

    def to_error(self) -> dict[str, Any]:
        return {
            "command": display_command(self.args),
            "cwd": self.cwd,
            "duration_ms": self.duration_ms,
            "error": self.error or self.stderr or self.stdout or "command failed",
            "returncode": self.returncode,
        }


def run_command(
    args: list[str],
    *,
    cwd: Path,
    timeout: float = 3.0,
    parse_json: bool = False,
    stdin_text: str | None = None,
) -> CommandResult:
    started = time.perf_counter()
    try:
        completed = subprocess.run(
            args,
            cwd=str(cwd),
            capture_output=True,
            text=True,
            input=stdin_text,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired:
        duration_ms = int((time.perf_counter() - started) * 1000)
        return CommandResult(
            ok=False,
            args=args,
            cwd=str(cwd),
            duration_ms=duration_ms,
            error=f"timed out after {timeout:.1f}s",
        )
    except OSError as exc:
        duration_ms = int((time.perf_counter() - started) * 1000)
        return CommandResult(
            ok=False,
            args=args,
            cwd=str(cwd),
            duration_ms=duration_ms,
            error=str(exc),
        )

    duration_ms = int((time.perf_counter() - started) * 1000)
    stdout = completed.stdout.strip()
    stderr = completed.stderr.strip()

    if completed.returncode != 0:
        return CommandResult(
            ok=False,
            args=args,
            cwd=str(cwd),
            duration_ms=duration_ms,
            stdout=stdout,
            stderr=stderr,
            error=stderr or stdout or f"exit {completed.returncode}",
            returncode=completed.returncode,
        )

    if parse_json:
        try:
            data = json.loads(stdout or "null")
        except json.JSONDecodeError as exc:
            return CommandResult(
                ok=False,
                args=args,
                cwd=str(cwd),
                duration_ms=duration_ms,
                stdout=stdout,
                stderr=stderr,
                error=f"invalid JSON: {exc}",
            )
        return CommandResult(
            ok=True,
            args=args,
            cwd=str(cwd),
            duration_ms=duration_ms,
            data=data,
        )

    return CommandResult(
        ok=True,
        args=args,
        cwd=str(cwd),
        duration_ms=duration_ms,
        data=stdout,
    )


def parse_services(status_text: str) -> list[str]:
    for line in status_text.splitlines():
        if line.startswith("Services:"):
            services = line.split("Services:", 1)[1].strip()
            return [chunk.strip() for chunk in re.split(r"\s{2,}", services) if chunk.strip()]
    return []


def parse_status_summary(status_text: str) -> dict[str, Any]:
    lines = [line.rstrip() for line in status_text.splitlines()]
    town = ""
    root_path = ""
    overseer = ""
    tmux_socket = ""

    for line in lines:
        if line.startswith("Town:"):
            town = line.split(":", 1)[1].strip()
        elif line.startswith("/") and not root_path:
            root_path = line.strip()
        elif line.startswith("👤 Overseer:"):
            overseer = line.split(":", 1)[1].strip()

    socket_match = re.search(r"tmux \(-L ([^,]+),", status_text)
    if socket_match:
        tmux_socket = socket_match.group(1)

    return {
        "town": town,
        "root_path": root_path,
        "overseer": overseer,
        "services": parse_services(status_text),
        "tmux_socket": tmux_socket,
        "raw": status_text,
    }


def parse_feed(text: str) -> list[dict[str, str]]:
    events: list[dict[str, str]] = []
    for raw_line in text.splitlines():
        line = raw_line.rstrip()
        if not line:
            continue
        match = EVENT_RE.match(line)
        if match:
            events.append(
                {
                    "time": match.group("time"),
                    "symbol": match.group("symbol"),
                    "actor": match.group("actor").strip(),
                    "message": match.group("message").strip(),
                    "raw": line,
                }
            )
        else:
            events.append(
                {
                    "time": "",
                    "symbol": "",
                    "actor": "",
                    "message": line,
                    "raw": line,
                }
            )
    return events


def configured_rig_names(gt_root: Path) -> list[str]:
    rigs_path = gt_root / "rigs.json"
    payload = parse_json_loose(read_file_text(rigs_path))
    rigs = payload.get("rigs") if isinstance(payload, dict) else {}
    if not isinstance(rigs, dict):
        return []
    names = [str(name) for name in rigs.keys() if str(name)]
    names.sort()
    return names


def discover_bead_stores(gt_root: Path) -> list[dict[str, Any]]:
    stores: list[dict[str, Any]] = []
    if (gt_root / ".beads").is_dir():
        stores.append({"name": "hq", "path": gt_root, "scope": "hq"})
    for rig_name in configured_rig_names(gt_root):
        child = gt_root / rig_name
        if not child.is_dir():
            continue
        if (child / ".beads").is_dir():
            stores.append({"name": rig_name, "path": child, "scope": rig_name})
    return stores


def normalize_change_path(path_text: str) -> str:
    text = str(path_text or "").strip()
    while text.startswith("./"):
        text = text[2:]
    return text


def is_benign_crew_change(path_text: str) -> bool:
    text = normalize_change_path(path_text)
    if not text:
        return False
    if text in {"gitignore", ".gitignore", ".beads"}:
        return True
    if text.startswith(".beads/"):
        return True
    return False


def enrich_crew_workspace(crew: dict[str, Any]) -> dict[str, Any]:
    enriched = dict(crew)
    modified = [str(path) for path in (enriched.get("git_modified") or []) if str(path)]
    untracked = [str(path) for path in (enriched.get("git_untracked") or []) if str(path)]
    benign_modified = [path for path in modified if is_benign_crew_change(path)]
    benign_untracked = [path for path in untracked if is_benign_crew_change(path)]
    risky_modified = [path for path in modified if not is_benign_crew_change(path)]
    risky_untracked = [path for path in untracked if not is_benign_crew_change(path)]

    if not modified and not untracked:
        git_state = "clean"
        git_status_label = "git clean"
        git_status_tone = "done"
    elif risky_modified or risky_untracked:
        git_state = "warning"
        git_status_label = "repo changes"
        git_status_tone = "stuck"
    else:
        git_state = "local_state"
        git_status_label = "local state only"
        git_status_tone = "memory"

    enriched.update(
        {
            "git_modified": modified,
            "git_untracked": untracked,
            "git_benign_modified": benign_modified,
            "git_benign_untracked": benign_untracked,
            "git_risky_modified": risky_modified,
            "git_risky_untracked": risky_untracked,
            "git_state": git_state,
            "git_status_label": git_status_label,
            "git_status_tone": git_status_tone,
            "git_has_risky_changes": bool(risky_modified or risky_untracked),
            "git_has_local_state_only": git_state == "local_state",
        }
    )
    return enriched


def merge_crews(all_crews: list[dict[str, Any]], running_crews: list[dict[str, Any]]) -> list[dict[str, Any]]:
    merged: dict[tuple[str, str], dict[str, Any]] = {}
    for item in all_crews:
        key = (item.get("rig", ""), item.get("name", ""))
        merged[key] = dict(item)

    for item in running_crews:
        key = (item.get("rig", ""), item.get("name", ""))
        base = merged.setdefault(key, {"rig": item.get("rig"), "name": item.get("name")})
        base.update(item)

    crews = [enrich_crew_workspace(item) for item in merged.values()]
    crews.sort(key=lambda item: (item.get("rig", ""), item.get("name", "")))
    return crews


def collect_polecats(gt_root: Path) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int]:
    # `gt polecat list --all --json` is one of the heaviest regular snapshot
    # commands once multiple rigs and persistent polecats exist. A 2s timeout
    # causes false "poll degraded" badges even when the town is healthy, so we
    # give this specific query a slightly larger budget.
    result = run_command(
        ["gt", "polecat", "list", "--all", "--json"],
        cwd=gt_root,
        timeout=6.0,
        parse_json=True,
    )
    duration_ms = result.duration_ms
    if not result.ok:
        return [], [result.to_error()], duration_ms
    polecats = result.data if isinstance(result.data, list) else []
    return polecats, [], duration_ms


def issue_is_merge(issue: dict[str, Any]) -> bool:
    labels = issue.get("labels") or []
    if "gt:merge-request" in labels:
        return True
    meta = parse_simple_metadata_block(issue.get("description") or "")
    return "source_issue" in meta and "commit_sha" in meta


def issue_is_system(issue: dict[str, Any]) -> bool:
    labels = issue.get("labels") or []
    issue_id = issue.get("id", "")
    title = issue.get("title", "")
    issue_type = issue.get("issue_type", "")
    if "gt:rig" in labels:
        return True
    if issue_type == "molecule":
        return True
    if issue_id.startswith("hq-wisp-"):
        return True
    if title.startswith("mol-"):
        return True
    return False


def issue_is_graph_noise(issue: dict[str, Any]) -> bool:
    issue_type = issue.get("issue_type", "")
    labels = set(issue.get("labels") or [])
    issue_id = issue.get("id", "")
    if issue_type and issue_type not in GRAPH_ALLOWED_TYPES and not issue_is_system(issue):
        return True
    if "gt:message" in labels or "gt:escalation" in labels:
        return True
    if issue_id.startswith("hq-cv-"):
        return True
    return False


def derive_ui_status(issue: dict[str, Any], *, blocked_ids: set[str], hooked_ids: set[str]) -> str:
    status = issue.get("status", "open")
    issue_id = issue.get("id", "")
    if status == "closed":
        return "done"
    if status in {"hooked", "in_progress"} or issue_id in hooked_ids:
        return "running"
    if status == "deferred":
        return "ice"
    if status == "blocked" or issue_id in blocked_ids:
        return "stuck"
    return "ready"


def compact_issue(issue: dict[str, Any], *, blocked_ids: set[str], hooked_ids: set[str]) -> dict[str, Any]:
    return {
        "id": issue.get("id", ""),
        "title": issue.get("title", ""),
        "description": issue.get("description", ""),
        "status": issue.get("status", ""),
        "ui_status": derive_ui_status(issue, blocked_ids=blocked_ids, hooked_ids=hooked_ids),
        "priority": issue.get("priority"),
        "type": issue.get("issue_type", ""),
        "owner": issue.get("owner", ""),
        "assignee": issue.get("assignee", ""),
        "created_at": issue.get("created_at", ""),
        "updated_at": issue.get("updated_at", ""),
        "closed_at": issue.get("closed_at", ""),
        "parent": issue.get("parent", ""),
        "labels": issue.get("labels", []),
        "dependency_count": issue.get("dependency_count", 0),
        "dependent_count": issue.get("dependent_count", 0),
        "blocked_by_count": issue.get("blocked_by_count", 0),
        "blocked_by": issue.get("blocked_by", []),
        "is_system": issue_is_system(issue),
    }


def make_repo_id(root: str) -> str:
    return f"repo-{zlib.crc32(root.encode('utf-8')) & 0xFFFFFFFF:x}"


def parse_tmux_target(gt_root: Path, pane_path: str) -> dict[str, str] | None:
    try:
        relative = Path(pane_path).resolve().relative_to(gt_root.resolve())
    except ValueError:
        return None

    parts = relative.parts
    if not parts:
        return None

    if parts[0] == "mayor":
        return {"target": "mayor", "role": "mayor", "scope": "hq", "label": "mayor"}
    if parts[0] == "deacon":
        if len(parts) >= 3 and parts[1] == "dogs" and parts[2] == "boot":
            return {"target": "boot", "role": "boot", "scope": "hq", "label": "boot"}
        return {"target": "deacon", "role": "deacon", "scope": "hq", "label": "deacon"}
    if len(parts) >= 2 and parts[1] == "witness":
        rig = parts[0]
        return {"target": f"{rig}/witness", "role": "witness", "scope": rig, "label": f"{rig}/witness"}
    if len(parts) >= 3 and parts[1] == "refinery" and parts[2] == "rig":
        rig = parts[0]
        return {"target": f"{rig}/refinery", "role": "refinery", "scope": rig, "label": f"{rig}/refinery"}
    if len(parts) >= 3 and parts[1] == "polecats":
        rig = parts[0]
        name = parts[2]
        return {
            "target": f"{rig}/polecats/{name}",
            "role": "polecat",
            "scope": rig,
            "label": f"{rig}/polecats/{name}",
        }
    if len(parts) >= 3 and parts[1] == "crew":
        rig = parts[0]
        name = parts[2]
        return {"target": f"{rig}/crew/{name}", "role": "crew", "scope": rig, "label": f"{rig}/crew/{name}"}
    return None


def collect_tmux_agents(gt_root: Path, tmux_socket: str) -> tuple[list[dict[str, Any]], list[dict[str, Any]], int]:
    errors: list[dict[str, Any]] = []
    duration_ms = 0
    agents: list[dict[str, Any]] = []
    if not tmux_socket:
        return agents, errors, duration_ms

    pane_result = run_command(
        [
            "tmux",
            "-L",
            tmux_socket,
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{window_name}|#{pane_id}|#{pane_current_path}|#{pane_current_command}",
        ],
        cwd=gt_root,
        timeout=2.0,
    )
    duration_ms += pane_result.duration_ms
    if not pane_result.ok:
        errors.append(pane_result.to_error())
        return agents, errors, duration_ms

    for line in pane_result.data.splitlines():
        session_name, _, pane_id, pane_path, pane_command = (line.split("|", 4) + ["", "", "", "", ""])[:5]
        target_meta = parse_tmux_target(gt_root, pane_path)
        if not target_meta or target_meta["role"] == "boot":
            continue
        agents.append(
            {
                "target": target_meta["target"],
                "label": target_meta["label"],
                "role": target_meta["role"],
                "scope": target_meta["scope"],
                "kind": "tmux",
                "session_name": session_name,
                "pane_id": pane_id,
                "current_path": pane_path,
                "current_command": pane_command,
                "has_session": True,
            }
        )

    return agents, errors, duration_ms


def collect_agents(
    gt_root: Path,
    status_summary: dict[str, Any],
    crews: list[dict[str, Any]],
    feed_events: list[dict[str, str]],
) -> tuple[list[dict[str, Any]], dict[str, list[str]], list[dict[str, Any]], int]:
    errors: list[dict[str, Any]] = []
    duration_ms = 0
    agents_by_target: dict[str, dict[str, Any]] = {}

    tmux_agents, tmux_errors, tmux_ms = collect_tmux_agents(gt_root, status_summary.get("tmux_socket", ""))
    errors.extend(tmux_errors)
    duration_ms += tmux_ms
    for agent in tmux_agents:
        agents_by_target[agent["target"]] = agent

    for crew in crews:
        target = f"{crew.get('rig')}/crew/{crew.get('name')}"
        existing = agents_by_target.get(target, {})
        existing.update(
            {
                "target": target,
                "label": target,
                "role": "crew",
                "scope": crew.get("rig", ""),
                "kind": existing.get("kind", "external"),
                "session_name": existing.get("session_name", ""),
                "pane_id": existing.get("pane_id", ""),
                "current_path": crew.get("path", existing.get("current_path", "")),
                "current_command": existing.get("current_command", ""),
                "has_session": bool(crew.get("has_session")),
                "crew": crew,
            }
        )
        agents_by_target[target] = existing

    polecats, polecat_errors, polecat_ms = collect_polecats(gt_root)
    errors.extend(polecat_errors)
    duration_ms += polecat_ms
    for polecat in polecats:
        target = f"{polecat.get('rig')}/polecats/{polecat.get('name')}"
        existing = agents_by_target.get(target, {})
        existing.update(
            {
                "target": target,
                "label": target,
                "role": "polecat",
                "scope": polecat.get("rig", ""),
                "kind": existing.get("kind", "external"),
                "session_name": existing.get("session_name", ""),
                "pane_id": existing.get("pane_id", ""),
                "current_path": existing.get("current_path", ""),
                "current_command": existing.get("current_command", ""),
                "has_session": bool(polecat.get("session_running")),
                "polecat": polecat,
                "runtime_state": polecat.get("state", ""),
            }
        )
        agents_by_target[target] = existing

    event_map: dict[str, list[dict[str, str]]] = {}
    task_event_map: dict[str, list[dict[str, str]]] = {}
    for index, event in enumerate(feed_events):
        actor = event.get("actor") or ""
        message = event.get("message") or ""
        if actor:
            event_map.setdefault(actor, []).append(event)
        slung_match = SLUNG_EVENT_RE.match(message)
        if slung_match:
            target = slung_match.group("target")
            task_event_map.setdefault(target, []).append(
                {
                    "kind": "assigned",
                    "task_id": slung_match.group("task"),
                    "time": event.get("time", ""),
                    "message": message,
                    "order": str(index),
                }
            )
        done_match = DONE_EVENT_RE.match(message)
        if done_match and actor:
            task_event_map.setdefault(actor, []).append(
                {
                    "kind": "done",
                    "task_id": done_match.group("task"),
                    "time": event.get("time", ""),
                    "message": message,
                    "order": str(index),
                }
            )

    hook_by_issue: dict[str, list[str]] = {}
    if agents_by_target:
        with ThreadPoolExecutor(max_workers=worker_count(len(agents_by_target))) as executor:
            future_map = {
                executor.submit(
                    run_command,
                    ["gt", "hook", "show", target, "--json"],
                    cwd=gt_root,
                    timeout=2.0,
                    parse_json=True,
                ): target
                for target in agents_by_target
            }
            for future in as_completed(future_map):
                target = future_map[future]
                agent = agents_by_target[target]
                hook_result = future.result()
                duration_ms += hook_result.duration_ms
                if not hook_result.ok:
                    errors.append(hook_result.to_error())
                    agent["hook"] = {"agent": target, "status": "unknown"}
                else:
                    agent["hook"] = hook_result.data
                    bead_id = (hook_result.data or {}).get("bead_id")
                    if bead_id:
                        hook_by_issue.setdefault(bead_id, []).append(target)

    for target, agent in agents_by_target.items():
        agent["events"] = event_map.get(target, [])
        agent["task_events"] = task_event_map.get(target, [])[-6:]
        agent["recent_task"] = agent["task_events"][-1] if agent["task_events"] else None

    agents = list(agents_by_target.values())
    agents.sort(key=lambda item: (item.get("scope", ""), item.get("role", ""), item.get("target", "")))
    return agents, hook_by_issue, errors, duration_ms


def collect_bead_data(
    gt_root: Path,
    bead_stores: list[dict[str, Any]],
    hook_by_issue: dict[str, list[str]],
) -> tuple[dict[str, Any], list[dict[str, Any]], int]:
    errors: list[dict[str, Any]] = []
    duration_ms = 0
    all_issues: dict[str, dict[str, Any]] = {}
    blocked_ids: set[str] = set()
    hooked_ids: set[str] = set(hook_by_issue.keys())
    store_summaries: list[dict[str, Any]] = []
    merge_links: list[dict[str, Any]] = []

    for store in bead_stores:
        store_path = Path(store["path"])
        status_result = run_command(["bd", "status", "--json"], cwd=store_path)
        all_result = run_command(
            ["bd", "list", "--all", "--json", "--limit", "300"],
            cwd=store_path,
            parse_json=True,
        )
        blocked_result = run_command(["bd", "blocked", "--json"], cwd=store_path, parse_json=True)
        hooked_result = run_command(["bd", "list", "--status=hooked", "--json"], cwd=store_path, parse_json=True)
        duration_ms += (
            status_result.duration_ms
            + all_result.duration_ms
            + blocked_result.duration_ms
            + hooked_result.duration_ms
        )

        for result in (status_result, all_result, blocked_result, hooked_result):
            if not result.ok:
                errors.append(result.to_error())

        status_payload = parse_json_loose(
            status_result.data if status_result.ok else (status_result.stdout or status_result.error or "")
        )
        issues = all_result.data if all_result.ok and isinstance(all_result.data, list) else []
        blocked = blocked_result.data if blocked_result.ok and isinstance(blocked_result.data, list) else []
        hooked = hooked_result.data if hooked_result.ok and isinstance(hooked_result.data, list) else []
        blocked_local_ids = {item.get("id", "") for item in blocked if item.get("id")}
        hooked_local_ids = {item.get("id", "") for item in hooked if item.get("id")}

        blocked_ids.update(blocked_local_ids)
        hooked_ids.update(hooked_local_ids)

        open_count = 0
        closed_count = 0
        for issue in issues:
            issue_id = issue.get("id")
            if not issue_id:
                continue
            issue["_store"] = store["name"]
            issue["_scope"] = store["scope"]
            all_issues[issue_id] = issue
            if issue.get("status") == "closed":
                closed_count += 1
            else:
                open_count += 1

            if issue_is_merge(issue):
                meta = parse_simple_metadata_block(issue.get("description") or "")
                source_issue = meta.get("source_issue")
                commit_sha = meta.get("commit_sha")
                if source_issue and commit_sha:
                    merge_links.append(
                        {
                            "task_id": source_issue,
                            "merge_issue_id": issue_id,
                            "commit_sha": commit_sha,
                            "short_sha": commit_sha[:7],
                            "branch": meta.get("branch", ""),
                            "target": meta.get("target", ""),
                            "worker": meta.get("worker", ""),
                            "store": store["name"],
                            "scope": store["scope"],
                            "title": issue.get("title", ""),
                        }
                    )

        store_summaries.append(
            {
                "name": store["name"],
                "scope": store["scope"],
                "path": str(store_path),
                "available": bool(status_result.ok),
                "summary": (status_payload or {}).get("summary", {}) if isinstance(status_payload, dict) else {},
                "error": (
                    (status_payload or {}).get("error", "")
                    if isinstance(status_payload, dict)
                    else (status_result.error if not status_result.ok else "")
                ),
                "exact_status_counts": count_issue_statuses(issues),
                "total": len(issues),
                "open": open_count,
                "closed": closed_count,
                "blocked": sum(1 for issue in issues if issue.get("id") in blocked_local_ids),
                "hooked": sum(1 for issue in issues if issue.get("id") in hooked_local_ids),
            }
        )

    nodes: list[dict[str, Any]] = []
    edges: list[dict[str, Any]] = []
    edge_keys: set[tuple[str, str, str]] = set()
    summary = {
        "ready": 0,
        "running": 0,
        "stuck": 0,
        "done": 0,
        "ice": 0,
        "system_running": 0,
    }

    for issue in all_issues.values():
        if issue_is_merge(issue):
            continue
        if issue_is_graph_noise(issue):
            continue
        node = compact_issue(issue, blocked_ids=blocked_ids, hooked_ids=hooked_ids)
        node.update(
            {
                "kind": "task",
                "scope": issue.get("_scope", ""),
                "agent_targets": hook_by_issue.get(issue.get("id", ""), []),
            }
        )
        if node["is_system"]:
            if node["ui_status"] == "running":
                summary["system_running"] += 1
        else:
            summary[node["ui_status"]] = summary.get(node["ui_status"], 0) + 1
        nodes.append(node)

    node_ids = {node["id"] for node in nodes}
    for issue in all_issues.values():
        if issue_is_merge(issue):
            continue
        if issue_is_graph_noise(issue):
            continue
        issue_id = issue.get("id", "")
        for dep in issue.get("dependencies") or []:
            source = dep.get("depends_on_id", "")
            if source not in node_ids or issue_id not in node_ids:
                continue
            edge_kind = "parent" if dep.get("type") == "parent-child" else "dependency"
            edge_key = (source, issue_id, edge_kind)
            if edge_key in edge_keys:
                continue
            edge_keys.add(edge_key)
            edges.append({"source": source, "target": issue_id, "kind": edge_kind})

    return {
        "issues": all_issues,
        "nodes": nodes,
        "edges": edges,
        "blocked_ids": sorted(blocked_ids),
        "hooked_ids": sorted(hooked_ids),
        "store_summaries": store_summaries,
        "merge_links": merge_links,
        "summary": summary,
    }, errors, duration_ms


def parse_git_status(text: str) -> dict[str, Any]:
    lines = text.splitlines()
    branch = lines[0][3:] if lines and lines[0].startswith("## ") else ""
    modified = 0
    untracked = 0
    for line in lines[1:]:
        if line.startswith("??"):
            untracked += 1
        elif line.strip():
            modified += 1
    return {
        "branch": branch,
        "modified": modified,
        "untracked": untracked,
        "dirty": bool(modified or untracked),
        "raw": text,
    }


def parse_git_log(text: str, repo_id: str, repo_label: str) -> list[dict[str, Any]]:
    commits: list[dict[str, Any]] = []
    for line in text.splitlines():
        parts = line.split("\x1f")
        if len(parts) != 5:
            continue
        sha, short_sha, committed_at, refs, subject = parts
        commits.append(
            {
                "repo_id": repo_id,
                "repo_label": repo_label,
                "sha": sha,
                "short_sha": short_sha,
                "committed_at": committed_at,
                "refs": refs,
                "subject": subject,
                "task_ids": find_issue_ids(subject),
            }
        )
    return commits


def parse_git_branches(text: str) -> list[dict[str, Any]]:
    branches: list[dict[str, Any]] = []
    for line in text.splitlines():
        parts = line.split("\x1f")
        if len(parts) != 5:
            continue
        head_flag, name, short_sha, committed_at, subject = parts
        branches.append(
            {
                "current": head_flag == "*",
                "name": name,
                "short_sha": short_sha,
                "committed_at": committed_at,
                "subject": subject,
            }
        )
    return branches


def parse_worktrees(text: str) -> list[dict[str, str]]:
    worktrees: list[dict[str, str]] = []
    current: dict[str, str] = {}
    for line in text.splitlines():
        if not line:
            if current:
                worktrees.append(current)
                current = {}
            continue
        key, _, value = line.partition(" ")
        current[key] = value
    if current:
        worktrees.append(current)
    return [
        {
            "path": item.get("worktree", ""),
            "head": item.get("HEAD", ""),
            "branch": item.get("branch", "").replace("refs/heads/", ""),
        }
        for item in worktrees
    ]


def collect_git_memory(
    gt_root: Path,
    crews: list[dict[str, Any]],
    merge_links: list[dict[str, Any]],
) -> tuple[dict[str, Any], list[dict[str, Any]], int]:
    errors: list[dict[str, Any]] = []
    duration_ms = 0
    candidates: list[tuple[str, Path, str]] = [("Town Root", gt_root, "hq")]
    for crew in crews:
        crew_path = Path(crew.get("path", ""))
        if crew_path:
            candidates.append((f"Crew {crew.get('rig')}/{crew.get('name')}", crew_path, crew.get("rig", "")))

    repo_roots: dict[str, dict[str, Any]] = {}
    for label, path, scope in candidates:
        top_result = run_command(["git", "-C", str(path), "rev-parse", "--show-toplevel"], cwd=gt_root)
        duration_ms += top_result.duration_ms
        if not top_result.ok:
            errors.append(top_result.to_error())
            continue
        root = top_result.data.strip()
        repo = repo_roots.setdefault(
            root,
            {
                "id": make_repo_id(root),
                "root": root,
                "labels": [],
                "scopes": [],
            },
        )
        if label not in repo["labels"]:
            repo["labels"].append(label)
        if scope and scope not in repo["scopes"]:
            repo["scopes"].append(scope)

    repos: list[dict[str, Any]] = []
    task_memory: dict[str, list[dict[str, Any]]] = {}
    all_recent_commits: list[dict[str, Any]] = []

    for root, repo_stub in repo_roots.items():
        repo_id = repo_stub["id"]
        label = " / ".join(repo_stub["labels"])
        scopes = repo_stub.get("scopes", [])
        repo_scope = scopes[0] if len(scopes) == 1 else ""

        status_result = run_command(["git", "-C", root, "status", "--short", "--branch"], cwd=gt_root)
        log_result = run_command(
            [
                "git",
                "-C",
                root,
                "log",
                "--date=iso-strict",
                "--decorate=short",
                "--format=%H%x1f%h%x1f%cI%x1f%D%x1f%s",
                "-n",
                "16",
            ],
            cwd=gt_root,
        )
        branch_result = run_command(
            [
                "git",
                "-C",
                root,
                "branch",
                "--format=%(HEAD)%x1f%(refname:short)%x1f%(objectname:short)%x1f%(committerdate:iso8601-strict)%x1f%(subject)",
                "--sort=-committerdate",
            ],
            cwd=gt_root,
        )
        worktree_result = run_command(["git", "-C", root, "worktree", "list", "--porcelain"], cwd=gt_root)
        duration_ms += (
            status_result.duration_ms
            + log_result.duration_ms
            + branch_result.duration_ms
            + worktree_result.duration_ms
        )

        for result in (status_result, log_result, branch_result, worktree_result):
            if not result.ok:
                errors.append(result.to_error())

        status = parse_git_status(status_result.data or "") if status_result.ok else {}
        recent_commits = parse_git_log(log_result.data or "", repo_id=repo_id, repo_label=label) if log_result.ok else []
        branches = parse_git_branches(branch_result.data or "") if branch_result.ok else []
        worktrees = parse_worktrees(worktree_result.data or "") if worktree_result.ok else []

        repos.append(
            {
                "id": repo_id,
                "label": label,
                "root": root,
                "scope": repo_scope,
                "scopes": scopes,
                "status": status,
                "recent_commits": recent_commits,
                "branches": branches[:12],
                "worktrees": worktrees,
            }
        )
        all_recent_commits.extend(recent_commits)

        for commit in recent_commits:
            for task_id in commit["task_ids"]:
                task_memory.setdefault(task_id, []).append(
                    {
                        "source": "commit-message",
                        "repo_id": repo_id,
                        "repo_label": label,
                        "sha": commit["sha"],
                        "short_sha": commit["short_sha"],
                        "subject": commit["subject"],
                        "committed_at": commit["committed_at"],
                        "scope": repo_scope,
                        "available_local": True,
                    }
                )

    repo_ids = {repo["id"] for repo in repos}
    for link in merge_links:
        entry = {
            "source": "merge-bead",
            "repo_id": "",
            "repo_label": "",
            "sha": link["commit_sha"],
            "short_sha": link["short_sha"],
            "subject": link["title"],
            "committed_at": "",
            "branch": link.get("branch", ""),
            "target": link.get("target", ""),
            "worker": link.get("worker", ""),
            "merge_issue_id": link.get("merge_issue_id", ""),
            "scope": link.get("scope", ""),
            "available_local": False,
        }
        task_memory.setdefault(link["task_id"], []).append(entry)

    all_recent_commits.sort(key=lambda item: item.get("committed_at", ""), reverse=True)
    for entries in task_memory.values():
        entries.sort(
            key=lambda item: (
                item.get("committed_at", ""),
                item.get("short_sha", ""),
            ),
            reverse=True,
        )

    return {
        "repos": repos,
        "recent_commits": all_recent_commits[:20],
        "task_memory": task_memory,
        "repo_ids": sorted(repo_ids),
    }, errors, duration_ms


def finalize_graph(
    bead_data: dict[str, Any],
    git_memory: dict[str, Any],
    convoy_data: dict[str, Any],
) -> dict[str, Any]:
    nodes = deep_copy_json(bead_data["nodes"])
    edges = deep_copy_json(bead_data["edges"])
    node_ids = {node["id"] for node in nodes}
    node_scope_map = {node["id"]: node.get("scope", "") for node in nodes}
    convoy_task_ids = {
        str(task_id)
        for task_id in ((convoy_data or {}).get("task_index") or {}).keys()
        if str(task_id)
    }

    for node in nodes:
        memory_entries = git_memory["task_memory"].get(node["id"], [])
        node["linked_commits"] = [entry.get("short_sha", "") for entry in memory_entries[:3]]
        node["linked_commit_count"] = len(memory_entries)

    edge_keys = {(edge["source"], edge["target"], edge["kind"]) for edge in edges}
    commit_nodes: list[dict[str, Any]] = []
    for task_id, entries in git_memory["task_memory"].items():
        if task_id not in node_ids:
            continue
        for entry in entries:
            sha = entry.get("sha", "")
            if not sha:
                continue
            commit_node_id = f"commit:{sha}"
            if commit_node_id not in node_ids:
                commit_nodes.append(
                    {
                        "id": commit_node_id,
                        "kind": "commit",
                        "title": entry.get("subject") or f"Commit {entry.get('short_sha', '')}",
                        "description": "",
                        "status": "memory",
                        "ui_status": "memory",
                        "priority": None,
                        "type": "commit",
                        "owner": "",
                        "assignee": "",
                        "created_at": "",
                        "updated_at": entry.get("committed_at", ""),
                        "closed_at": "",
                        "parent": task_id,
                        "labels": [],
                        "dependency_count": 0,
                        "dependent_count": 0,
                        "blocked_by_count": 0,
                        "blocked_by": [],
                        "is_system": False,
                        "scope": entry.get("scope", "") or node_scope_map.get(task_id, ""),
                        "agent_targets": [],
                        "linked_commits": [],
                        "linked_commit_count": 0,
                        "sha": sha,
                        "short_sha": entry.get("short_sha", ""),
                        "repo_id": entry.get("repo_id", ""),
                        "repo_label": entry.get("repo_label", ""),
                        "branch": entry.get("branch", ""),
                        "available_local": bool(entry.get("available_local")),
                    }
                )
                node_ids.add(commit_node_id)
            edge_key = (task_id, commit_node_id, "commit")
            if edge_key not in edge_keys:
                edge_keys.add(edge_key)
                edges.append({"source": task_id, "target": commit_node_id, "kind": "commit"})

    nodes.extend(commit_nodes)

    interesting_ids = {edge["source"] for edge in edges} | {edge["target"] for edge in edges}
    interesting_ids.update(convoy_task_ids)
    interesting_ids.update(
        node["id"]
        for node in nodes
        if node.get("agent_targets")
        or node.get("linked_commit_count")
        or node.get("ui_status") in {"running", "stuck"}
        or (node.get("is_system") and node.get("ui_status") in {"ready", "running", "stuck"})
    )

    filtered_nodes: list[dict[str, Any]] = []
    kept_ids: set[str] = set()
    for node in nodes:
        if node["kind"] == "commit":
            if node.get("parent") not in interesting_ids:
                continue
        elif node["id"] not in interesting_ids:
            continue
        filtered_nodes.append(node)
        kept_ids.add(node["id"])

    filtered_edges = [
        edge
        for edge in edges
        if edge["source"] in kept_ids and edge["target"] in kept_ids
    ]
    return {"nodes": filtered_nodes, "edges": filtered_edges}


def build_activity_groups(
    agents: list[dict[str, Any]],
    graph_nodes: list[dict[str, Any]],
    task_memory: dict[str, list[dict[str, Any]]],
) -> dict[str, Any]:
    node_map = {node["id"]: node for node in graph_nodes}
    groups: dict[str, dict[str, Any]] = {}
    unassigned_agents: list[dict[str, Any]] = []

    for agent in agents:
        hook = agent.get("hook") or {}
        bead_id = hook.get("bead_id", "")
        node = node_map.get(bead_id)

        agent_payload = {
            "target": agent.get("target", ""),
            "label": agent.get("label", ""),
            "role": agent.get("role", ""),
            "scope": agent.get("scope", ""),
            "kind": agent.get("kind", ""),
            "has_session": bool(agent.get("has_session")),
            "runtime_state": agent.get("runtime_state", ""),
            "current_path": agent.get("current_path", ""),
            "session_name": agent.get("session_name", ""),
            "hook": hook,
            "events": agent.get("events", [])[-6:],
            "crew": agent.get("crew", {}),
            "polecat": agent.get("polecat", {}),
        }

        if not bead_id:
            if (
                agent_payload["events"]
                or agent_payload["has_session"]
                or agent_payload["runtime_state"]
            ):
                unassigned_agents.append(agent_payload)
            continue

        group = groups.setdefault(
            bead_id,
            {
                "task_id": bead_id,
                "title": (node or {}).get("title", hook.get("title", bead_id)),
                "stored_status": (node or {}).get("status", hook.get("status", "")),
                "ui_status": (node or {}).get("ui_status", hook.get("status", "running")),
                "is_system": bool((node or {}).get("is_system", hook.get("title", "").startswith("mol-"))),
                "scope": (node or {}).get("scope", agent.get("scope", "")),
                "agents": [],
                "events": [],
                "memory": task_memory.get(bead_id, []),
            },
        )
        group["agents"].append(agent_payload)
        group["events"].extend(agent_payload["events"])

    task_groups = list(groups.values())
    for group in task_groups:
        group["events"] = group["events"][-10:]
        group["agent_count"] = len(group["agents"])

    def sort_key(group: dict[str, Any]) -> tuple[int, int, str]:
        status_order = {"running": 0, "stuck": 1, "ready": 2, "ice": 3, "done": 4, "memory": 5}
        return (
            1 if group.get("is_system") else 0,
            status_order.get(group.get("ui_status", "ready"), 9),
            group.get("task_id", ""),
        )

    task_groups.sort(key=sort_key)
    unassigned_agents.sort(key=lambda item: item.get("target", ""))
    return {"groups": task_groups, "unassigned_agents": unassigned_agents}


def collect_convoy_data(gt_root: Path) -> tuple[dict[str, Any], list[dict[str, Any]], int]:
    result = run_command(["bd", "list", "--type=convoy", "--all", "--json"], cwd=gt_root, parse_json=True)
    duration_ms = result.duration_ms
    errors: list[dict[str, Any]] = []
    if not result.ok:
        errors.append(result.to_error())
        return {"convoys": [], "task_index": {}}, errors, duration_ms

    raw_convoys = result.data if isinstance(result.data, list) else []
    convoys: list[dict[str, Any]] = []
    task_index: dict[str, dict[str, Any]] = {}

    for raw in raw_convoys:
        if not isinstance(raw, dict):
            continue
        convoy_id = str(raw.get("id") or "")
        status = str(raw.get("status") or "")
        title = str(raw.get("title") or "")
        tracked = raw.get("tracked") if isinstance(raw.get("tracked"), list) else []
        dependencies = raw.get("dependencies") if isinstance(raw.get("dependencies"), list) else []
        is_closed = status == "closed"
        tracked_ids: list[str] = []

        tracked_items = tracked
        if not tracked_items:
            tracked_items = [
                item
                for item in dependencies
                if isinstance(item, dict) and str(item.get("type") or item.get("dependency_type") or "") == "tracks"
            ]

        for item in tracked_items:
            if not isinstance(item, dict):
                continue
            task_id = str(item.get("id") or item.get("depends_on_id") or item.get("issue_id") or "")
            if not task_id:
                continue
            tracked_ids.append(task_id)
            entry = task_index.setdefault(
                task_id,
                {
                    "total": 0,
                    "open": 0,
                    "closed": 0,
                    "convoy_ids": [],
                },
            )
            entry["total"] += 1
            if is_closed:
                entry["closed"] += 1
            else:
                entry["open"] += 1
            if convoy_id and convoy_id not in entry["convoy_ids"]:
                entry["convoy_ids"].append(convoy_id)

        convoys.append(
            {
                "id": convoy_id,
                "title": title,
                "status": status,
                "tracked_ids": tracked_ids,
                "completed": int(raw.get("completed") or (len(tracked_ids) if is_closed else 0)),
                "total": int(raw.get("total") or len(tracked_ids)),
            }
        )

    for entry in task_index.values():
        entry["all_closed"] = bool(entry["total"]) and entry["open"] == 0

    return {"convoys": convoys, "task_index": task_index}, errors, duration_ms


def build_snapshot(gt_root: Path, action_history: list[dict[str, Any]]) -> dict[str, Any]:
    started = time.perf_counter()
    errors: list[dict[str, Any]] = []

    status_result = run_command(["gt", "status", "--fast"], cwd=gt_root)
    vitals_result = run_command(["gt", "vitals"], cwd=gt_root)
    crew_list_result = run_command(["gt", "crew", "list", "--all", "--json"], cwd=gt_root, parse_json=True)
    crew_status_result = run_command(["gt", "crew", "status", "--json"], cwd=gt_root, parse_json=True)
    feed_result = run_command(
        ["gt", "feed", "--plain", "--since", "20m", "--limit", "80", "--no-follow"],
        cwd=gt_root,
    )

    common_results = [status_result, vitals_result, crew_list_result, crew_status_result, feed_result]
    for result in common_results:
        if not result.ok:
            errors.append(result.to_error())

    status_summary = parse_status_summary(status_result.data or "") if status_result.ok else {}
    crews = merge_crews(
        crew_list_result.data if crew_list_result.ok and isinstance(crew_list_result.data, list) else [],
        crew_status_result.data if crew_status_result.ok and isinstance(crew_status_result.data, list) else [],
    )
    feed_events = parse_feed(feed_result.data or "") if feed_result.ok else []

    agents, hook_by_issue, agent_errors, agent_ms = collect_agents(
        gt_root=gt_root,
        status_summary=status_summary,
        crews=crews,
        feed_events=feed_events,
    )
    errors.extend(agent_errors)

    bead_data, bead_errors, bead_ms = collect_bead_data(
        gt_root=gt_root,
        bead_stores=discover_bead_stores(gt_root),
        hook_by_issue=hook_by_issue,
    )
    errors.extend(bead_errors)

    git_memory, git_errors, git_ms = collect_git_memory(
        gt_root=gt_root,
        crews=crews,
        merge_links=bead_data["merge_links"],
    )
    errors.extend(git_errors)

    convoy_data, convoy_errors, convoy_ms = collect_convoy_data(gt_root)
    errors.extend(convoy_errors)

    graph = finalize_graph(bead_data=bead_data, git_memory=git_memory, convoy_data=convoy_data)
    activity = build_activity_groups(
        agents=agents,
        graph_nodes=graph["nodes"],
        task_memory=git_memory["task_memory"],
    )

    task_nodes = [node for node in graph["nodes"] if node["kind"] == "task" and not node.get("is_system")]
    system_task_nodes = [node for node in graph["nodes"] if node["kind"] == "task" and node.get("is_system")]

    summary = {
        "running_tasks": sum(1 for node in task_nodes if node.get("ui_status") == "running"),
        "stuck_tasks": sum(1 for node in task_nodes if node.get("ui_status") == "stuck"),
        "ready_tasks": sum(1 for node in task_nodes if node.get("ui_status") == "ready"),
        "done_tasks": sum(1 for node in task_nodes if node.get("ui_status") == "done"),
        "system_running": sum(1 for node in system_task_nodes if node.get("ui_status") == "running"),
        "active_agents": sum(1 for agent in agents if agent.get("has_session")),
        "task_groups": len(activity["groups"]),
        "repos": len(git_memory["repos"]),
        "command_errors": len(errors),
        "stored_status_counts": count_issue_statuses(task_nodes),
        "derived_status_counts": {
            "running": sum(1 for node in task_nodes if node.get("ui_status") == "running"),
            "stuck": sum(1 for node in task_nodes if node.get("ui_status") == "stuck"),
            "ready": sum(1 for node in task_nodes if node.get("ui_status") == "ready"),
            "done": sum(1 for node in task_nodes if node.get("ui_status") == "done"),
            "ice": sum(1 for node in task_nodes if node.get("ui_status") == "ice"),
        },
    }

    alerts: list[str] = []
    if any("daemon (stopped)" in service for service in status_summary.get("services", [])):
        alerts.append("Gas Town daemon is stopped.")
    if summary["running_tasks"] == 0 and summary["ready_tasks"] > 0 and summary["active_agents"] > 0:
        alerts.append("Agents are alive, but no product tasks are currently running.")
    if summary["stuck_tasks"] > 0:
        alerts.append(f"{summary['stuck_tasks']} task node(s) are dependency-blocked.")
    risky_crews = [crew for crew in crews if crew.get("git_has_risky_changes")]
    if risky_crews:
        alerts.append(f"{len(risky_crews)} crew workspace(s) have risky repo changes.")

    snapshot = {
        "generated_at": now_iso(),
        "generation_ms": int((time.perf_counter() - started) * 1000),
        "gt_root": str(gt_root),
        "status": status_summary,
        "vitals_raw": vitals_result.data if vitals_result.ok else (vitals_result.error or ""),
        "status_legend": STATUS_LEGEND,
        "summary": summary,
        "alerts": alerts,
        "graph": graph,
        "activity": activity,
        "git": git_memory,
        "convoys": convoy_data,
        "crews": crews,
        "agents": agents,
        "stores": bead_data["store_summaries"],
        "actions": action_history[:12],
        "errors": errors,
        "timings": {
            "gt_commands_ms": sum(result.duration_ms for result in common_results),
            "agent_commands_ms": agent_ms,
            "bd_commands_ms": bead_ms,
            "git_commands_ms": git_ms,
            "convoy_commands_ms": convoy_ms,
        },
    }
    return snapshot


class SnapshotStore:
    def __init__(self, gt_root: Path, interval_seconds: float) -> None:
        self.gt_root = gt_root
        self.interval_seconds = interval_seconds
        self.lock = threading.Lock()
        self.codex_lock = threading.Lock()
        self.claude_lock = threading.Lock()
        self.action_lock = threading.Lock()
        self.action_history: list[dict[str, Any]] = []
        self.codex_rollout_list_cache: dict[str, Any] = {"expires_at": 0.0, "files": []}
        self.codex_rollout_meta_cache: dict[str, dict[str, Any]] = {}
        self.codex_transcript_cache: dict[str, dict[str, Any]] = {}
        self.claude_session_list_cache: dict[str, Any] = {"expires_at": 0.0, "files": []}
        self.claude_session_meta_cache: dict[str, dict[str, Any]] = {}
        self.claude_transcript_cache: dict[str, dict[str, Any]] = {}
        self.snapshot: dict[str, Any] = {
            "generated_at": now_iso(),
            "generation_ms": 0,
            "gt_root": str(gt_root),
            "status": {},
            "vitals_raw": "",
            "status_legend": STATUS_LEGEND,
            "summary": {},
            "alerts": [],
            "graph": {"nodes": [], "edges": []},
            "activity": {"groups": [], "unassigned_agents": []},
            "git": {"repos": [], "recent_commits": [], "task_memory": {}},
            "convoys": {"convoys": [], "task_index": {}},
            "crews": [],
            "agents": [],
            "stores": [],
            "actions": [],
            "errors": [],
            "timings": {},
        }
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, name="gtui-snapshot", daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=1)

    def get(self) -> dict[str, Any]:
        with self.lock:
            return deep_copy_json(self.snapshot)

    def refresh_once(self) -> None:
        snapshot = build_snapshot(self.gt_root, action_history=self.action_history)
        with self.lock:
            self.snapshot = snapshot

    def _record_action(self, action: dict[str, Any]) -> None:
        with self.lock:
            self.action_history.insert(0, action)
            self.action_history = self.action_history[:24]

    def get_repo_root(self, repo_id: str) -> str | None:
        with self.lock:
            for repo in self.snapshot.get("git", {}).get("repos", []):
                if repo.get("id") == repo_id:
                    return repo.get("root")
        return None

    def get_node(self, node_id: str) -> dict[str, Any] | None:
        with self.lock:
            for node in self.snapshot.get("graph", {}).get("nodes", []):
                if node.get("id") == node_id:
                    return deep_copy_json(node)
        return None

    def get_agent(self, target: str) -> dict[str, Any] | None:
        with self.lock:
            for agent in self.snapshot.get("agents", []):
                if agent.get("target") == target:
                    return deep_copy_json(agent)
        return None

    def get_tmux_socket(self) -> str:
        with self.lock:
            return str(self.snapshot.get("status", {}).get("tmux_socket", "") or "")

    def get_services(self) -> list[str]:
        with self.lock:
            services = self.snapshot.get("status", {}).get("services", [])
            if not isinstance(services, list):
                return []
            return deep_copy_json(services)

    def list_recent_codex_rollouts(self) -> list[Path]:
        now = time.time()
        with self.codex_lock:
            cached_files = self.codex_rollout_list_cache.get("files", [])
            expires_at = float(self.codex_rollout_list_cache.get("expires_at", 0.0) or 0.0)
            if cached_files and expires_at > now:
                return [Path(path_text) for path_text in cached_files]

        files: list[Path] = []
        if CODEX_SESSIONS_ROOT.is_dir():
            try:
                files = sorted(
                    CODEX_SESSIONS_ROOT.glob("**/rollout-*.jsonl"),
                    key=lambda path: path.stat().st_mtime,
                    reverse=True,
                )[:CODEX_ROLLOUT_SCAN_LIMIT]
            except OSError:
                files = []

        with self.codex_lock:
            self.codex_rollout_list_cache = {
                "expires_at": now + CODEX_ROLLOUT_LIST_TTL_SECONDS,
                "files": [str(path) for path in files],
            }
        return files

    def get_codex_rollout_meta(self, path: Path) -> dict[str, Any]:
        try:
            stat = path.stat()
        except OSError:
            return {}

        cache_key = str(path)
        signature = (stat.st_mtime_ns, stat.st_size)
        with self.codex_lock:
            cached = self.codex_rollout_meta_cache.get(cache_key)
            if cached and cached.get("signature") == signature:
                return deep_copy_json(cached.get("meta") or {})

        meta: dict[str, Any] = {
            "path": str(path),
            "cwd": "",
            "session_id": "",
            "modified_at": datetime.fromtimestamp(stat.st_mtime).astimezone().isoformat(timespec="seconds"),
            "mtime": stat.st_mtime,
        }
        for record in iter_jsonl_records(read_file_head(path, CODEX_ROLLOUT_HEAD_BYTES)):
            record_type = str(record.get("type") or "")
            payload = record.get("payload") if isinstance(record.get("payload"), dict) else {}
            if record_type == "session_meta":
                meta["session_id"] = str(payload.get("id") or meta["session_id"])
                meta["cwd"] = str(payload.get("cwd") or meta["cwd"])
            elif record_type == "turn_context" and not meta["cwd"]:
                meta["cwd"] = str(record.get("cwd") or payload.get("cwd") or "")
            if meta["cwd"] and meta["session_id"]:
                break

        meta["cwd"] = normalize_path_value(str(meta.get("cwd") or ""))
        with self.codex_lock:
            self.codex_rollout_meta_cache[cache_key] = {"signature": signature, "meta": deep_copy_json(meta)}
        return meta

    def find_codex_rollout(self, current_path: str) -> dict[str, Any] | None:
        best_meta: dict[str, Any] | None = None
        best_score = -1
        for path in self.list_recent_codex_rollouts():
            meta = self.get_codex_rollout_meta(path)
            score = match_path_score(current_path, str(meta.get("cwd") or ""))
            if score < 0:
                continue
            modified_at = float(meta.get("mtime") or 0.0)
            if score > best_score:
                best_score = score
                best_meta = meta
                continue
            if score == best_score and best_meta and modified_at > float(best_meta.get("mtime") or 0.0):
                best_meta = meta
        return deep_copy_json(best_meta) if best_meta else None

    def parse_codex_transcript(self, path_text: str) -> dict[str, Any]:
        path = Path(path_text)
        try:
            stat = path.stat()
        except OSError:
            return {}

        cache_key = str(path)
        signature = (stat.st_mtime_ns, stat.st_size)
        with self.codex_lock:
            cached = self.codex_transcript_cache.get(cache_key)
            if cached and cached.get("signature") == signature:
                return deep_copy_json(cached.get("view") or {})

        items: list[dict[str, Any]] = []
        call_map: dict[str, dict[str, str]] = {}
        for record in iter_jsonl_records(read_file_text(path)):
            timestamp = str(record.get("timestamp") or "")
            record_type = str(record.get("type") or "")
            payload = record.get("payload") if isinstance(record.get("payload"), dict) else {}

            if record_type == "response_item":
                payload_type = str(payload.get("type") or "")
                if payload_type == "message":
                    role = str(payload.get("role") or "")
                    if role not in {"assistant", "user"}:
                        continue
                    text = extract_message_text(payload.get("content"))
                    if not text or is_hidden_transcript_message(text):
                        continue
                    items.append(
                        {
                            "kind": role,
                            "phase": str(payload.get("phase") or ""),
                            "text": text,
                            "time": format_timestamp_short(timestamp),
                            "timestamp": timestamp,
                        }
                    )
                elif payload_type == "function_call":
                    tool_name = str(payload.get("name") or "")
                    call_id = str(payload.get("call_id") or "")
                    summary = summarize_tool_call(tool_name, payload.get("arguments"))
                    if call_id:
                        call_map[call_id] = {"tool": tool_name, "summary": summary}
                    items.append(
                        {
                            "kind": "tool_call",
                            "tool": tool_name,
                            "summary": summary,
                            "call_id": call_id,
                            "time": format_timestamp_short(timestamp),
                            "timestamp": timestamp,
                        }
                    )
                elif payload_type == "function_call_output":
                    call_id = str(payload.get("call_id") or "")
                    tool_info = call_map.get(call_id, {})
                    output_text = stringify_tool_output(payload.get("output"))
                    items.append(
                        {
                            "kind": "tool_output",
                            "tool": str(tool_info.get("tool") or ""),
                            "summary": summarize_tool_output(output_text),
                            "text": excerpt_tool_output(output_text),
                            "call_id": call_id,
                            "time": format_timestamp_short(timestamp),
                            "timestamp": timestamp,
                        }
                    )
                elif payload_type == "reasoning":
                    reasoning_item = {
                        "kind": "reasoning",
                        "summary": "Thinking...",
                        "time": format_timestamp_short(timestamp),
                        "timestamp": timestamp,
                    }
                    if items and items[-1].get("kind") == "reasoning":
                        items[-1] = reasoning_item
                    else:
                        items.append(reasoning_item)
        if items:
            trailing_reasoning = items[-1] if items[-1].get("kind") == "reasoning" else None
            items = [item for item in items if item.get("kind") != "reasoning"]
            if trailing_reasoning:
                items.append(trailing_reasoning)
        view = {
            "available": bool(items),
            "provider": "codex",
            "source": "codex-rollout",
            "session_file": str(path),
            "session_name": path.name,
            "revision": f"{stat.st_mtime_ns}:{stat.st_size}",
            "updated_at": datetime.fromtimestamp(stat.st_mtime).astimezone().isoformat(timespec="seconds"),
            "items": items,
        }
        with self.codex_lock:
            self.codex_transcript_cache[cache_key] = {"signature": signature, "view": deep_copy_json(view)}
        return view

    def get_codex_view(self, agent: dict[str, Any]) -> dict[str, Any]:
        current_command = str(agent.get("current_command", "") or "")
        current_path = str(agent.get("current_path", "") or "")
        if not is_codex_command(current_command) or not current_path:
            return {}
        rollout = self.find_codex_rollout(current_path)
        if not rollout:
            return {}
        view = self.parse_codex_transcript(str(rollout.get("path") or ""))
        if not view.get("available"):
            return {}
        view["cwd"] = str(rollout.get("cwd") or "")
        view["session_id"] = str(rollout.get("session_id") or "")
        return view

    def list_recent_claude_sessions(self, current_path: str) -> list[Path]:
        encoded = encode_claude_project_dir(current_path)
        if encoded:
            project_dir = CLAUDE_PROJECTS_ROOT / encoded
            if project_dir.is_dir():
                try:
                    return sorted(
                        project_dir.glob("*.jsonl"),
                        key=lambda path: path.stat().st_mtime,
                        reverse=True,
                    )[:CLAUDE_SESSION_SCAN_LIMIT]
                except OSError:
                    return []

        now = time.time()
        with self.claude_lock:
            cached_files = self.claude_session_list_cache.get("files", [])
            expires_at = float(self.claude_session_list_cache.get("expires_at", 0.0) or 0.0)
            if cached_files and expires_at > now:
                return [Path(path_text) for path_text in cached_files]

        files: list[Path] = []
        if CLAUDE_PROJECTS_ROOT.is_dir():
            try:
                files = sorted(
                    CLAUDE_PROJECTS_ROOT.glob("**/*.jsonl"),
                    key=lambda path: path.stat().st_mtime,
                    reverse=True,
                )[:CLAUDE_SESSION_SCAN_LIMIT]
            except OSError:
                files = []

        with self.claude_lock:
            self.claude_session_list_cache = {
                "expires_at": now + CLAUDE_SESSION_LIST_TTL_SECONDS,
                "files": [str(path) for path in files],
            }
        return files

    def get_claude_session_meta(self, path: Path) -> dict[str, Any]:
        try:
            stat = path.stat()
        except OSError:
            return {}

        cache_key = str(path)
        signature = (stat.st_mtime_ns, stat.st_size)
        with self.claude_lock:
            cached = self.claude_session_meta_cache.get(cache_key)
            if cached and cached.get("signature") == signature:
                return deep_copy_json(cached.get("meta") or {})

        meta: dict[str, Any] = {
            "path": str(path),
            "cwd": "",
            "session_id": path.stem,
            "modified_at": datetime.fromtimestamp(stat.st_mtime).astimezone().isoformat(timespec="seconds"),
            "mtime": stat.st_mtime,
        }
        for record in iter_jsonl_records(read_file_head(path, CLAUDE_SESSION_HEAD_BYTES)):
            if record.get("cwd") and not meta["cwd"]:
                meta["cwd"] = str(record.get("cwd") or "")
            if record.get("sessionId"):
                meta["session_id"] = str(record.get("sessionId") or meta["session_id"])
            if meta["cwd"] and meta["session_id"]:
                break

        meta["cwd"] = normalize_path_value(str(meta.get("cwd") or ""))
        with self.claude_lock:
            self.claude_session_meta_cache[cache_key] = {"signature": signature, "meta": deep_copy_json(meta)}
        return meta

    def find_claude_session(self, current_path: str) -> dict[str, Any] | None:
        best_meta: dict[str, Any] | None = None
        best_score = -1
        for path in self.list_recent_claude_sessions(current_path):
            meta = self.get_claude_session_meta(path)
            score = match_path_score(current_path, str(meta.get("cwd") or ""))
            if score < 0:
                continue
            modified_at = float(meta.get("mtime") or 0.0)
            if score > best_score:
                best_score = score
                best_meta = meta
                continue
            if score == best_score and best_meta and modified_at > float(best_meta.get("mtime") or 0.0):
                best_meta = meta
        return deep_copy_json(best_meta) if best_meta else None

    def parse_claude_transcript(self, path_text: str) -> dict[str, Any]:
        path = Path(path_text)
        try:
            stat = path.stat()
        except OSError:
            return {}

        cache_key = str(path)
        signature = (stat.st_mtime_ns, stat.st_size)
        with self.claude_lock:
            cached = self.claude_transcript_cache.get(cache_key)
            if cached and cached.get("signature") == signature:
                return deep_copy_json(cached.get("view") or {})

        items: list[dict[str, Any]] = []
        call_map: dict[str, dict[str, str]] = {}
        last_model = ""
        for record in iter_jsonl_records(read_file_text(path)):
            if record.get("isSidechain") is True:
                continue
            timestamp = str(record.get("timestamp") or "")
            record_type = str(record.get("type") or "")
            message = record.get("message") if isinstance(record.get("message"), dict) else {}
            content = message.get("content")

            if record_type == "user":
                if isinstance(content, str):
                    text = content.strip()
                    if text:
                        items.append(
                            {
                                "kind": "user",
                                "text": text,
                                "time": format_timestamp_short(timestamp),
                                "timestamp": timestamp,
                            }
                        )
                    continue
                if not isinstance(content, list):
                    continue
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    block_type = str(block.get("type") or "")
                    if block_type == "tool_result":
                        call_id = str(block.get("tool_use_id") or "")
                        tool_info = call_map.get(call_id, {})
                        output_text = stringify_claude_tool_output(record.get("toolUseResult"), block.get("content"))
                        items.append(
                            {
                                "kind": "tool_output",
                                "tool": str(tool_info.get("tool") or ""),
                                "summary": summarize_tool_output(output_text),
                                "text": excerpt_tool_output(output_text),
                                "call_id": call_id,
                                "is_error": bool(block.get("is_error")),
                                "time": format_timestamp_short(timestamp),
                                "timestamp": timestamp,
                            }
                        )
                    elif block_type == "text":
                        text = extract_claude_text_block(block)
                        if text:
                            items.append(
                                {
                                    "kind": "user",
                                    "text": text,
                                    "time": format_timestamp_short(timestamp),
                                    "timestamp": timestamp,
                                }
                            )
                continue

            if record_type == "assistant":
                model = str(message.get("model") or "")
                if model:
                    last_model = model
                if isinstance(content, str):
                    text = content.strip()
                    if text:
                        items.append(
                            {
                                "kind": "assistant",
                                "text": text,
                                "model": model,
                                "time": format_timestamp_short(timestamp),
                                "timestamp": timestamp,
                            }
                        )
                    continue
                if not isinstance(content, list):
                    continue
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    block_type = str(block.get("type") or "")
                    if block_type == "text":
                        text = extract_claude_text_block(block)
                        if text:
                            items.append(
                                {
                                    "kind": "assistant",
                                    "text": text,
                                    "model": model,
                                    "time": format_timestamp_short(timestamp),
                                    "timestamp": timestamp,
                                }
                            )
                    elif block_type == "tool_use":
                        tool_name = str(block.get("name") or "")
                        call_id = str(block.get("id") or "")
                        tool_input = block.get("input") if isinstance(block.get("input"), dict) else {}
                        summary = summarize_tool_call(tool_name, tool_input)
                        if call_id:
                            call_map[call_id] = {"tool": tool_name, "summary": summary}
                        items.append(
                            {
                                "kind": "tool_call",
                                "tool": tool_name,
                                "summary": summary,
                                "call_id": call_id,
                                "time": format_timestamp_short(timestamp),
                                "timestamp": timestamp,
                            }
                        )
                    elif block_type == "thinking":
                        reasoning_item = {
                            "kind": "reasoning",
                            "summary": "Thinking...",
                            "time": format_timestamp_short(timestamp),
                            "timestamp": timestamp,
                        }
                        if items and items[-1].get("kind") == "reasoning":
                            items[-1] = reasoning_item
                        else:
                            items.append(reasoning_item)
                continue

            # Claude Code writes internal lifecycle metadata such as
            # stop_hook_summary and turn_duration into the JSONL. Those are not
            # user-visible transcript content.
            if record_type == "system":
                continue

        if items:
            trailing_reasoning = items[-1] if items[-1].get("kind") == "reasoning" else None
            items = [item for item in items if item.get("kind") != "reasoning"]
            if trailing_reasoning:
                items.append(trailing_reasoning)

        view = {
            "available": bool(items),
            "provider": "claude",
            "source": "claude-session",
            "session_file": str(path),
            "session_name": path.name,
            "model": last_model,
            "revision": f"{stat.st_mtime_ns}:{stat.st_size}",
            "updated_at": datetime.fromtimestamp(stat.st_mtime).astimezone().isoformat(timespec="seconds"),
            "items": items,
        }
        with self.claude_lock:
            self.claude_transcript_cache[cache_key] = {"signature": signature, "view": deep_copy_json(view)}
        return view

    def get_claude_view(self, agent: dict[str, Any]) -> dict[str, Any]:
        current_command = str(agent.get("current_command", "") or "")
        current_path = str(agent.get("current_path", "") or "")
        if not current_path:
            return {}
        if current_command and not is_claude_command(current_command):
            return {}
        claude_session = self.find_claude_session(current_path)
        if not claude_session:
            return {}
        view = self.parse_claude_transcript(str(claude_session.get("path") or ""))
        if not view.get("available"):
            return {}
        view["cwd"] = str(claude_session.get("cwd") or "")
        view["session_id"] = str(claude_session.get("session_id") or "")
        return view

    def get_transcript_view(self, agent: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
        codex_view = self.get_codex_view(agent)
        if codex_view.get("available"):
            return codex_view, codex_view, {}
        claude_view = self.get_claude_view(agent)
        if claude_view.get("available"):
            return claude_view, {}, claude_view
        return {}, {}, {}

    def discover_agent(self, target: str) -> dict[str, Any] | None:
        cached_agent = self.get_agent(target)
        tmux_socket = self.get_tmux_socket()
        if not tmux_socket:
            return cached_agent

        live_agents, _, _ = collect_tmux_agents(self.gt_root, tmux_socket)
        live_agent = next((item for item in live_agents if item.get("target") == target), None)
        if not live_agent:
            return cached_agent
        if not cached_agent:
            return live_agent

        merged = deep_copy_json(cached_agent)
        merged.update(live_agent)
        return merged

    def get_terminal_state(self, target: str) -> dict[str, Any]:
        agent = self.discover_agent(target)
        if not agent:
            raise ValueError(f"Unknown terminal target: {target}")

        tmux_socket = self.get_tmux_socket()
        tmux_target = str(agent.get("pane_id", "") or agent.get("session_name", "") or "")
        log_lines: list[str] = []
        capture_error = ""
        transcript_view, codex_view, claude_view = self.get_transcript_view(agent)
        if tmux_socket and tmux_target and not transcript_view.get("available"):
            capture_result = run_command(
                ["tmux", "-L", tmux_socket, "capture-pane", "-p", "-t", tmux_target, "-S", "-240"],
                cwd=self.gt_root,
                timeout=1.0,
            )
            if capture_result.ok:
                log_lines = normalize_lines(capture_result.data, limit=240)
            else:
                capture_error = capture_result.error

        events = deep_copy_json(agent.get("events", [])[-6:] if isinstance(agent.get("events"), list) else [])
        feed_result = run_command(
            ["gt", "feed", "--plain", "--since", "5m", "--limit", "40", "--no-follow"],
            cwd=self.gt_root,
            timeout=1.0,
        )
        if feed_result.ok:
            fresh_events = [event for event in parse_feed(feed_result.data or "") if event.get("actor") == target]
            if fresh_events:
                events = fresh_events[-6:]

        return {
            "target": agent.get("target", target),
            "label": agent.get("label", target),
            "role": agent.get("role", ""),
            "scope": agent.get("scope", ""),
            "kind": agent.get("kind", ""),
            "has_session": bool(agent.get("has_session")),
            "runtime_state": agent.get("runtime_state", ""),
            "current_path": agent.get("current_path", ""),
            "session_name": agent.get("session_name", ""),
            "pane_id": agent.get("pane_id", ""),
            "current_command": agent.get("current_command", ""),
            "hook": deep_copy_json(agent.get("hook") or {}),
            "events": events,
            "task_events": deep_copy_json(agent.get("task_events", [])[-6:] if isinstance(agent.get("task_events"), list) else []),
            "recent_task": deep_copy_json(agent.get("recent_task") or {}),
            "log_lines": log_lines,
            "codex_view": codex_view,
            "claude_view": claude_view,
            "transcript_view": transcript_view,
            "render_mode": str(transcript_view.get("provider") or "terminal") if transcript_view.get("available") else "terminal",
            "services": self.get_services(),
            "capture_error": capture_error,
            "generated_at": now_iso(),
        }

    def retry_task(self, task_id: str) -> dict[str, Any]:
        with self.action_lock:
            node = self.get_node(task_id)
            if not node:
                raise ValueError(f"Unknown task: {task_id}")

            command: list[str]
            if node.get("status") == "hooked" or node.get("ui_status") == "running":
                agent_targets = node.get("agent_targets") or []
                if not agent_targets:
                    raise ValueError(f"Task {task_id} is marked running but no hooked agent was found.")
                command = ["gt", "unsling", task_id, agent_targets[0], "--force"]
            elif node.get("status") == "in_progress":
                command = ["gt", "release", task_id, "-r", "GTUI retry requested"]
            else:
                raise ValueError(f"Task {task_id} is not in a retryable running state.")

            result = run_command(command, cwd=self.gt_root, timeout=4.0)
            action = {
                "kind": "retry-task",
                "task_id": task_id,
                "command": display_command(command),
                "ok": result.ok,
                "output": result.data if result.ok else result.error,
                "timestamp": now_iso(),
            }
            self._record_action(action)
            self.refresh_once()
            return action

    def pause_agent(self, target: str) -> dict[str, Any]:
        with self.action_lock:
            message = (
                "Pause after your current step. Do not take new work or mutate state "
                "until further instruction from GTUI. Reply with a short status summary."
            )
            command = ["gt", "nudge", target, "--mode", "wait-idle", "--message", message]
            result = run_command(command, cwd=self.gt_root, timeout=4.0)
            action = {
                "kind": "pause-agent",
                "target": target,
                "command": display_command(command),
                "ok": result.ok,
                "output": result.data if result.ok else result.error,
                "timestamp": now_iso(),
            }
            self._record_action(action)
            self.refresh_once()
            return action

    def inject_instruction(self, target: str, message: str) -> dict[str, Any]:
        with self.action_lock:
            if not message.strip():
                raise ValueError("Instruction message is empty.")
            command = ["gt", "nudge", target, "--mode", "wait-idle", "--message", message]
            result = run_command(command, cwd=self.gt_root, timeout=4.0)
            action = {
                "kind": "inject-instruction",
                "target": target,
                "command": display_command(command),
                "ok": result.ok,
                "output": result.data if result.ok else result.error,
                "timestamp": now_iso(),
            }
            self._record_action(action)
            self.refresh_once()
            return action

    def write_terminal(self, target: str, message: str) -> dict[str, Any]:
        with self.action_lock:
            if not message.strip():
                raise ValueError("Terminal message is empty.")

            agent = self.discover_agent(target)
            if not agent:
                raise ValueError(f"Unknown terminal target: {target}")
            if not agent.get("has_session"):
                raise ValueError(f"{target} does not currently have a live tmux session.")

            tmux_socket = self.get_tmux_socket()
            pane_id = str(agent.get("pane_id", "") or "")
            session_name = str(agent.get("session_name", "") or "")
            current_command = str(agent.get("current_command", "") or "")
            tmux_target = pane_id or session_name
            if not tmux_socket or not tmux_target:
                raise ValueError(f"No live tmux pane is known for {target}. Refresh the page and try again.")

            last_result: CommandResult | None = None
            last_command: list[str] = []
            if is_codex_command(current_command):
                load_buffer_command = ["tmux", "-L", tmux_socket, "load-buffer", "-"]
                last_command = load_buffer_command
                last_result = run_command(
                    load_buffer_command,
                    cwd=self.gt_root,
                    timeout=2.0,
                    stdin_text=message,
                )
                if last_result.ok:
                    codex_commands = [
                        ["tmux", "-L", tmux_socket, "paste-buffer", "-d", "-p", "-t", tmux_target],
                        ["tmux", "-L", tmux_socket, "send-keys", "-t", tmux_target, "Escape"],
                        ["tmux", "-L", tmux_socket, "send-keys", "-t", tmux_target, "Enter"],
                    ]
                    for command in codex_commands:
                        last_command = command
                        last_result = run_command(command, cwd=self.gt_root, timeout=2.0)
                        if not last_result.ok:
                            break
            else:
                lines = message.splitlines() or [message]
                for line in lines:
                    if line:
                        send_text_command = ["tmux", "-L", tmux_socket, "send-keys", "-t", tmux_target, "-l", line]
                        last_command = send_text_command
                        last_result = run_command(send_text_command, cwd=self.gt_root, timeout=2.0)
                        if not last_result.ok:
                            break

                    enter_command = ["tmux", "-L", tmux_socket, "send-keys", "-t", tmux_target, "Enter"]
                    last_command = enter_command
                    last_result = run_command(enter_command, cwd=self.gt_root, timeout=2.0)
                    if not last_result.ok:
                        break

            if last_result is None:
                raise ValueError("Nothing was sent to the terminal.")

            if not last_result.ok:
                error_text = last_result.error or ""
                if "can't find window" in error_text or "can't find pane" in error_text:
                    raise ValueError(
                        f"{target} has a stale tmux pane reference. Refresh GTUI and try again."
                    )
                raise ValueError(error_text)

            action = {
                "kind": "write-terminal",
                "target": target,
                "command": display_command(last_command),
                "ok": True,
                "output": f"Sent to {target}",
                "timestamp": now_iso(),
                "terminal": self.get_terminal_state(target),
            }
            self._record_action(action)
            return action

    def fetch_diff(self, repo_id: str, sha: str) -> dict[str, Any]:
        repo_root = self.get_repo_root(repo_id)
        if not repo_root:
            raise ValueError(f"Unknown repo id: {repo_id}")
        result = run_command(
            [
                "git",
                "-C",
                repo_root,
                "show",
                "--stat",
                "--patch",
                "--find-renames",
                "--format=fuller",
                "--no-ext-diff",
                sha,
            ],
            cwd=self.gt_root,
            timeout=5.0,
        )
        if not result.ok:
            raise ValueError(result.error)
        text = result.data or ""
        lines = text.splitlines()
        truncated = False
        if len(lines) > 500:
            lines = lines[:500]
            lines.append("")
            lines.append("[gtui] diff truncated to 500 lines")
            truncated = True
        return {
            "repo_id": repo_id,
            "sha": sha,
            "text": "\n".join(lines),
            "truncated": truncated,
        }

    def _loop(self) -> None:
        while not self._stop.is_set():
            self.refresh_once()
            self._stop.wait(self.interval_seconds)


class GTUIHandler(BaseHTTPRequestHandler):
    store: SnapshotStore

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path in {"/", "/index.html"}:
            self._serve_file(APP_ROOT / "index.html", HTML_HEADERS)
            return
        if parsed.path.startswith("/static/"):
            self._serve_static(parsed.path)
            return
        if parsed.path == "/api/snapshot":
            payload = json.dumps(self.store.get()).encode("utf-8")
            self._respond(HTTPStatus.OK, payload, JSON_HEADERS)
            return
        if parsed.path == "/api/terminal":
            query = parse_qs(parsed.query)
            target = query.get("target", [""])[0]
            try:
                payload = json.dumps(self.store.get_terminal_state(target)).encode("utf-8")
                self._respond(HTTPStatus.OK, payload, JSON_HEADERS)
            except ValueError as exc:
                self._respond_json_error(HTTPStatus.BAD_REQUEST, str(exc))
            return
        if parsed.path == "/api/git/diff":
            query = parse_qs(parsed.query)
            repo_id = query.get("repo", [""])[0]
            sha = query.get("sha", [""])[0]
            try:
                payload = json.dumps(self.store.fetch_diff(repo_id=repo_id, sha=sha)).encode("utf-8")
                self._respond(HTTPStatus.OK, payload, JSON_HEADERS)
            except ValueError as exc:
                self._respond_json_error(HTTPStatus.BAD_REQUEST, str(exc))
            return
        if parsed.path == "/healthz":
            self._respond(HTTPStatus.OK, b"ok\n", TEXT_HEADERS)
            return
        self._respond(HTTPStatus.NOT_FOUND, b"not found\n", TEXT_HEADERS)

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        try:
            body = self._parse_json_body()
        except ValueError as exc:
            self._respond_json_error(HTTPStatus.BAD_REQUEST, str(exc))
            return

        try:
            if parsed.path == "/api/action/retry-task":
                result = self.store.retry_task(body.get("task_id", ""))
            elif parsed.path == "/api/action/pause-agent":
                result = self.store.pause_agent(body.get("target", ""))
            elif parsed.path == "/api/action/inject":
                result = self.store.inject_instruction(body.get("target", ""), body.get("message", ""))
            elif parsed.path == "/api/action/write-terminal":
                result = self.store.write_terminal(body.get("target", ""), body.get("message", ""))
            else:
                self._respond_json_error(HTTPStatus.NOT_FOUND, "unknown action")
                return
        except ValueError as exc:
            self._respond_json_error(HTTPStatus.BAD_REQUEST, str(exc))
            return

        payload = json.dumps(result).encode("utf-8")
        self._respond(HTTPStatus.OK, payload, JSON_HEADERS)

    def log_message(self, format: str, *args: Any) -> None:
        return

    def _parse_json_body(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        if not raw:
            return {}
        try:
            data = json.loads(raw.decode("utf-8"))
        except json.JSONDecodeError as exc:
            raise ValueError(f"invalid JSON body: {exc}") from exc
        if not isinstance(data, dict):
            raise ValueError("JSON body must be an object")
        return data

    def _serve_file(self, path: Path, headers: dict[str, str]) -> None:
        if not path.is_file():
            self._respond(HTTPStatus.NOT_FOUND, b"not found\n", TEXT_HEADERS)
            return
        self._respond(HTTPStatus.OK, path.read_bytes(), headers)

    def _serve_static(self, request_path: str) -> None:
        relative_text = unquote(request_path.removeprefix("/static/"))
        relative_path = Path(relative_text)
        if (
            relative_path.is_absolute()
            or not relative_path.parts
            or any(part in {"", ".", ".."} for part in relative_path.parts)
        ):
            self._respond(HTTPStatus.NOT_FOUND, b"not found\n", TEXT_HEADERS)
            return

        try:
            path = (STATIC_ROOT / relative_path).resolve()
            path.relative_to(STATIC_ROOT.resolve())
        except (OSError, ValueError):
            self._respond(HTTPStatus.NOT_FOUND, b"not found\n", TEXT_HEADERS)
            return

        headers = {
            "Content-Type": STATIC_CONTENT_TYPES.get(path.suffix.lower(), "application/octet-stream"),
            "Cache-Control": "no-store",
        }
        self._serve_file(path, headers)

    def _respond(self, status: HTTPStatus, payload: bytes, headers: dict[str, str]) -> None:
        self.send_response(status.value)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _respond_json_error(self, status: HTTPStatus, message: str) -> None:
        payload = json.dumps({"error": message}).encode("utf-8")
        self._respond(status, payload, JSON_HEADERS)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="GTUI local dashboard server")
    parser.add_argument("--host", default="127.0.0.1", help="Address to bind")
    parser.add_argument("--port", type=int, default=8420, help="HTTP port")
    parser.add_argument(
        "--gt-root",
        default=os.environ.get("GT_ROOT", str(DEFAULT_GT_ROOT)),
        help="Path to the Gas Town workspace",
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=1.0,
        help="Snapshot refresh interval in seconds",
    )
    return parser.parse_args()


def main() -> None:
    ensure_local_bin_on_path()

    args = parse_args()
    gt_root = Path(args.gt_root).expanduser().resolve()
    if not gt_root.exists():
        raise SystemExit(f"GT root does not exist: {gt_root}")

    store = SnapshotStore(gt_root=gt_root, interval_seconds=max(args.interval, 0.5))
    GTUIHandler.store = store
    server = ThreadingHTTPServer((args.host, args.port), GTUIHandler)
    print(f"GTUI serving http://{args.host}:{args.port}")
    print(f"GT root: {gt_root}")
    store.start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        store.stop()


if __name__ == "__main__":
    main()
