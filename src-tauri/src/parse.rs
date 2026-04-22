//! Minimal parsers ported from `webui/server.py`.
//!
//! Only the pieces used by `build_snapshot` / `SnapshotStore` in this round:
//! `gt status` summary, `gt feed` events, and the path helpers used by the
//! session-match scoring. Everything else (`collect_agents`, `finalize_graph`,
//! etc.) stays in Python for now and is stubbed from the Rust side.

use std::path::Path;
use std::sync::OnceLock;

use chrono::{Local, SecondsFormat};
use regex::Regex;
use serde_json::{json, Value};

use crate::models::StatusSummary;

/// `datetime.now().astimezone().isoformat(timespec="seconds")` in Python.
pub fn now_iso() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn services_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s{2,}").expect("static regex"))
}

fn tmux_socket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"tmux \(-L ([^,]+),").expect("static regex"))
}

fn event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\[(?P<time>[^\]]+)\]\s+(?P<symbol>\S+)\s+(?P<actor>.+?)\s{2,}(?P<message>.+)$",
        )
        .expect("static regex")
    })
}

/// Split a `Services: a  b  c` line into individual service strings.
pub fn parse_services(status_text: &str) -> Vec<String> {
    for line in status_text.lines() {
        if let Some(tail) = line.strip_prefix("Services:") {
            return services_re()
                .split(tail.trim())
                .map(str::trim)
                .filter(|chunk| !chunk.is_empty())
                .map(String::from)
                .collect();
        }
    }
    Vec::new()
}

/// Parse the output of `gt status --fast`.
pub fn parse_status_summary(status_text: &str) -> StatusSummary {
    let mut summary = StatusSummary {
        raw: status_text.to_string(),
        ..StatusSummary::default()
    };

    for raw_line in status_text.lines() {
        let line = raw_line.trim_end();
        if let Some(rest) = line.strip_prefix("Town:") {
            summary.town = rest.trim().to_string();
        } else if line.starts_with('/') && summary.root_path.is_empty() {
            summary.root_path = line.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("👤 Overseer:") {
            summary.overseer = rest.trim().to_string();
        }
    }

    if let Some(caps) = tmux_socket_re().captures(status_text) {
        summary.tmux_socket = caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default();
    }

    summary.services = parse_services(status_text);
    summary
}

/// Parse `gt feed` plain output into a list of loose event dicts matching the
/// Python shape: `{time, symbol, actor, message, raw}`.
pub fn parse_feed(text: &str) -> Vec<Value> {
    let mut events = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some(caps) = event_re().captures(line) {
            events.push(json!({
                "time": caps.name("time").map(|m| m.as_str()).unwrap_or(""),
                "symbol": caps.name("symbol").map(|m| m.as_str()).unwrap_or(""),
                "actor": caps.name("actor").map(|m| m.as_str().trim()).unwrap_or(""),
                "message": caps.name("message").map(|m| m.as_str().trim()).unwrap_or(""),
                "raw": line,
            }));
        } else {
            events.push(json!({
                "time": "",
                "symbol": "",
                "actor": "",
                "message": line,
                "raw": line,
            }));
        }
    }
    events
}

/// Port of `normalize_lines` from Python: rstrip each line, trim leading and
/// trailing blank lines, and cap to the last `limit` lines.
pub fn normalize_lines(text: &str, limit: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| line.trim_end_matches(['\r', ' ', '\t']).to_string())
        .collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() > limit {
        let start = lines.len() - limit;
        lines = lines.split_off(start);
    }
    lines
}

/// Normalise a filesystem path for comparison. Matches `normalize_path_value`
/// in Python: prefer `realpath` (symlink resolution) then `normpath`.
pub fn normalize_path_value(path_text: &str) -> String {
    if path_text.is_empty() {
        return String::new();
    }
    let p = Path::new(path_text);
    let canonical = std::fs::canonicalize(p)
        .map(|buf| buf.to_string_lossy().into_owned())
        .unwrap_or_else(|_| normpath(path_text));
    if canonical.is_empty() {
        normpath(path_text)
    } else {
        canonical
    }
}

/// Stand-in for Python's `os.path.normpath`. We only need the behaviour for
/// POSIX-style paths since that's where Gas Town runs. Handles trailing
/// slashes, `.` and `..` segments.
fn normpath(path_text: &str) -> String {
    let is_abs = path_text.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for segment in path_text.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                if matches!(stack.last(), Some(&"..")) || stack.is_empty() {
                    if !is_abs {
                        stack.push("..");
                    }
                } else {
                    stack.pop();
                }
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        if is_abs {
            "/".to_string()
        } else {
            ".".to_string()
        }
    } else if is_abs {
        format!("/{}", stack.join("/"))
    } else {
        stack.join("/")
    }
}

/// Encode a filesystem path the way Claude Code does for its project-dir
/// buckets under `~/.claude/projects/`: replace every separator with `-`.
pub fn encode_claude_project_dir(path_text: &str) -> String {
    let normalized = normalize_path_value(path_text);
    if normalized.is_empty() {
        return String::new();
    }
    normalized.replace('/', "-")
}

/// Score how well `session_cwd` matches `target_path`. Negative means no
/// match; higher is better. Matches `match_path_score` in Python.
pub fn match_path_score(target_path: &str, session_cwd: &str) -> i32 {
    let target = normalize_path_value(target_path);
    let session = normalize_path_value(session_cwd);
    if target.is_empty() || session.is_empty() {
        return -1;
    }
    if target == session {
        return 4000 + session.len() as i32;
    }
    let target_sep = format!("{}/", target);
    if session.starts_with(&target_sep) {
        return 3000 + session.len() as i32;
    }
    let session_sep = format!("{}/", session);
    if target.starts_with(&session_sep) {
        return 2000 + session.len() as i32;
    }
    -1
}

/// Cheap helper for tests that need a path stringified identically to the
/// Python port (`str(Path(...))`).
#[cfg(test)]
pub(crate) fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Stringify a path shared with the snapshot layer.
pub fn pathbuf_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_services_splits_on_wide_gaps() {
        let text =
            "Town: gastown\nServices:   daemon (running)  witness (running)  dolt (stopped)\n";
        assert_eq!(
            parse_services(text),
            vec![
                "daemon (running)".to_string(),
                "witness (running)".to_string(),
                "dolt (stopped)".to_string(),
            ]
        );
    }

    #[test]
    fn parse_services_empty_when_missing() {
        assert!(parse_services("Town: gastown\n").is_empty());
    }

    #[test]
    fn parse_status_summary_matches_python_fields() {
        let text = "\
Town: gastown
/home/user/gt
👤 Overseer: mayor
Services:   daemon (running)  dolt (running)
tmux (-L gastown, foo)
";
        let summary = parse_status_summary(text);
        assert_eq!(summary.town, "gastown");
        assert_eq!(summary.root_path, "/home/user/gt");
        assert_eq!(summary.overseer, "mayor");
        assert_eq!(summary.tmux_socket, "gastown");
        assert_eq!(summary.services.len(), 2);
        assert_eq!(summary.raw, text);
    }

    #[test]
    fn parse_status_summary_handles_missing_fields() {
        let text = "Town: gastown\n";
        let summary = parse_status_summary(text);
        assert_eq!(summary.town, "gastown");
        assert_eq!(summary.root_path, "");
        assert_eq!(summary.overseer, "");
        assert_eq!(summary.tmux_socket, "");
        assert!(summary.services.is_empty());
    }

    #[test]
    fn parse_feed_recognises_structured_events() {
        let text = "\
[10:12:33] ◐ gtui/polecats/furiosa  started gui-bn8.4
[10:12:34] ● gtui/witness  heartbeat
raw passthrough line
";
        let events = parse_feed(text);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["actor"], "gtui/polecats/furiosa");
        assert_eq!(events[0]["message"], "started gui-bn8.4");
        assert_eq!(events[2]["actor"], "");
        assert_eq!(events[2]["message"], "raw passthrough line");
        assert_eq!(events[2]["symbol"], "");
    }

    #[test]
    fn parse_feed_skips_blank_lines() {
        let events = parse_feed("\n\n\n");
        assert!(events.is_empty());
    }

    #[test]
    fn normpath_collapses_dot_segments() {
        assert_eq!(normpath("/a/b/./c/../d/"), "/a/b/d");
        assert_eq!(normpath("a/./b/../c"), "a/c");
        assert_eq!(normpath("/"), "/");
        assert_eq!(normpath(""), ".");
    }

    #[test]
    fn match_path_score_prefers_exact_match() {
        let exact = match_path_score("/home/user/gt", "/home/user/gt");
        let descendant = match_path_score("/home/user/gt", "/home/user/gt/subdir");
        let ancestor = match_path_score("/home/user/gt/subdir", "/home/user/gt");
        assert!(exact > descendant);
        assert!(descendant > ancestor);
        assert!(ancestor > 0);
    }

    #[test]
    fn match_path_score_negative_when_unrelated() {
        assert_eq!(match_path_score("/a/b", "/c/d"), -1);
        assert_eq!(match_path_score("", "/c/d"), -1);
        assert_eq!(match_path_score("/a/b", ""), -1);
    }

    #[test]
    fn encode_claude_project_dir_replaces_separators() {
        // normalize_path_value may canonicalize; on paths that don't exist it
        // falls back to our POSIX normpath. Either way the separator shape is
        // mapped to dashes.
        let encoded = encode_claude_project_dir("/tmp/does-not-exist/gtui");
        assert!(encoded.starts_with('-'), "got {encoded:?}");
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn now_iso_is_rfc3339_seconds() {
        let stamp = now_iso();
        // Loose shape check: `YYYY-MM-DDTHH:MM:SS±HH:MM`.
        assert!(
            stamp.len() >= 19,
            "expected iso-8601-ish string, got {stamp:?}"
        );
        assert!(stamp.contains('T'), "expected 'T' separator in {stamp:?}");
    }

    #[test]
    fn path_str_and_pathbuf_str_agree() {
        let p = std::path::PathBuf::from("/tmp/example");
        assert_eq!(path_str(p.as_path()), pathbuf_str(p.as_path()));
    }

    #[test]
    fn normalize_lines_strips_and_caps() {
        let lines = normalize_lines("\n\n  first  \nsecond\n   \n", 16);
        assert_eq!(lines, vec!["  first".to_string(), "second".to_string()]);
    }

    #[test]
    fn normalize_lines_keeps_tail_when_over_limit() {
        let text = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = normalize_lines(&text, 5);
        assert_eq!(
            lines,
            vec![
                "line 16".to_string(),
                "line 17".to_string(),
                "line 18".to_string(),
                "line 19".to_string(),
                "line 20".to_string(),
            ]
        );
    }
}
