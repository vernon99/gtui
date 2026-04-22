//! Snapshot assembly + store.
//!
//! Port of `build_snapshot()` and `SnapshotStore` from `webui/server.py`. The
//! five coarse `gt` CLI calls are fanned out in parallel via `tokio::join!`
//! (Python's `run_command` is blocking and sequential), but the downstream
//! collectors (`collect_agents`, `collect_bead_data`, `collect_git_memory`,
//! `collect_convoy_data`, `finalize_graph`, `build_activity_groups`) are
//! stubbed here — they're tracked as follow-on beads. Everything the store
//! *does* own — locks, polling loop, action ring buffer, caches — is in place.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::command::{display_command, run_command, RunOptions};
use crate::config::POLL_INTERVAL;
use crate::models::{
    default_status_legend, Activity, AgentInfo, Metrics, StatusSummary, Timings, WorkspaceSnapshot,
};
use crate::parse::{now_iso, parse_feed, parse_status_summary};
use crate::sessions::{
    claude_projects_root, find_claude_session, parse_claude_transcript, ClaudeCache, CodexCache,
};

/// Maximum number of most-recent actions retained on the snapshot store.
pub const ACTION_HISTORY_LIMIT: usize = 24;

/// Number of actions surfaced on each snapshot (matches Python `[:12]`).
pub const SNAPSHOT_ACTION_LIMIT: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalSendMode {
    CodexPaste,
    ClaudePaste,
    LineKeys,
}

fn normalize_command_name(command: &str) -> String {
    Path::new(command.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn terminal_send_mode(command: &str) -> TerminalSendMode {
    match normalize_command_name(command).as_str() {
        "codex" => TerminalSendMode::CodexPaste,
        "claude" | "claude.exe" | "node" => TerminalSendMode::ClaudePaste,
        _ => TerminalSendMode::LineKeys,
    }
}

/// Cached git diff entry. `text` is pre-truncated (≤ 500 lines plus a trailing
/// marker) just like the Python side so downstream consumers don't re-pay the
/// cost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedGitDiff {
    pub repo_id: String,
    pub sha: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Default)]
struct SnapshotState {
    snapshot: WorkspaceSnapshot,
    action_history: VecDeque<Value>,
    /// Running hash of the last frame, used for change-detection ("skip
    /// re-render if unchanged"). Cheap `u64` derived from the JSON encoding of
    /// the fields the UI actually cares about.
    last_fingerprint: u64,
}

/// Shared inner state reachable from both the background poller and the
/// command handlers. Kept separate from the outer `SnapshotStore` so that the
/// public handle can be cheaply cloned.
pub struct SnapshotStoreInner {
    gt_root: PathBuf,
    interval: Duration,
    state: Mutex<SnapshotState>,
    codex_cache: Mutex<CodexCache>,
    claude_cache: Mutex<ClaudeCache>,
    git_diff_cache: Mutex<HashMap<(String, String), CachedGitDiff>>,
    shutdown: Notify,
}

/// Cloneable handle to the snapshot store. All I/O runs on the background
/// Tokio task; callers obtain typed views via the accessors on this struct.
#[derive(Clone)]
pub struct SnapshotStore {
    inner: Arc<SnapshotStoreInner>,
}

impl SnapshotStore {
    /// Build a store rooted at `gt_root`. The polling loop is not started
    /// until [`SnapshotStore::spawn`] is called.
    pub fn new(gt_root: impl Into<PathBuf>) -> Self {
        Self::with_interval(gt_root, POLL_INTERVAL)
    }

    /// Same as [`SnapshotStore::new`] but with a caller-chosen cadence. Tests
    /// use this with `Duration::ZERO` to step the loop manually.
    pub fn with_interval(gt_root: impl Into<PathBuf>, interval: Duration) -> Self {
        let gt_root = gt_root.into();
        let initial = WorkspaceSnapshot {
            generated_at: now_iso(),
            gt_root: gt_root.to_string_lossy().into_owned(),
            status_legend: default_status_legend(),
            ..WorkspaceSnapshot::default()
        };
        let state = SnapshotState {
            snapshot: initial,
            action_history: VecDeque::new(),
            last_fingerprint: 0,
        };
        Self {
            inner: Arc::new(SnapshotStoreInner {
                gt_root,
                interval,
                state: Mutex::new(state),
                codex_cache: Mutex::new(CodexCache::default()),
                claude_cache: Mutex::new(ClaudeCache::default()),
                git_diff_cache: Mutex::new(HashMap::new()),
                shutdown: Notify::new(),
            }),
        }
    }

    /// Root path the store queries `gt`/`bd`/`git` in.
    pub fn gt_root(&self) -> &Path {
        &self.inner.gt_root
    }

    /// Return a clone of the current snapshot (safe to hand to serde / IPC
    /// handlers without holding the state lock).
    pub fn get(&self) -> WorkspaceSnapshot {
        self.inner
            .state
            .lock()
            .expect("snapshot state lock poisoned")
            .snapshot
            .clone()
    }

    /// Return a clone of the current action history.
    pub fn action_history(&self) -> Vec<Value> {
        self.inner
            .state
            .lock()
            .expect("snapshot state lock poisoned")
            .action_history
            .iter()
            .cloned()
            .collect()
    }

    /// Record a new action and prune to the last [`ACTION_HISTORY_LIMIT`]
    /// entries. Mirrors `SnapshotStore._record_action` in Python: newest first.
    pub fn record_action(&self, action: Value) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("snapshot state lock poisoned");
        state.action_history.push_front(action);
        while state.action_history.len() > ACTION_HISTORY_LIMIT {
            state.action_history.pop_back();
        }
    }

    /// Build a fresh snapshot and install it if its fingerprint differs from
    /// the previous one. Returns `true` if the snapshot was updated.
    pub async fn refresh_once(&self) -> bool {
        let history = self.action_history();
        let snapshot = build_snapshot(&self.inner.gt_root, &history).await;
        self.install_snapshot(snapshot)
    }

    /// Install a pre-built snapshot if its fingerprint differs from the
    /// previously stored frame. Returns `true` when the snapshot was actually
    /// written. Factored out of `refresh_once` so tests can exercise the
    /// fingerprint-dedup path without depending on `gt` CLI behaviour.
    pub fn install_snapshot(&self, snapshot: WorkspaceSnapshot) -> bool {
        let fingerprint = fingerprint_snapshot(&snapshot);
        let mut state = self
            .inner
            .state
            .lock()
            .expect("snapshot state lock poisoned");
        if state.last_fingerprint == fingerprint && state.last_fingerprint != 0 {
            return false;
        }
        state.snapshot = snapshot;
        state.last_fingerprint = fingerprint;
        true
    }

    /// Spawn the background polling task. Returns a handle the caller can
    /// hold onto; dropping it does not stop the loop — use [`SnapshotStore::shutdown`].
    pub fn spawn(&self) -> JoinHandle<()> {
        let inner = Arc::clone(&self.inner);
        let handle = self.clone();
        tokio::spawn(async move {
            loop {
                // Refresh. Panics inside refresh_once would otherwise abort the
                // polling loop forever; the store is resilient to individual
                // failed gt/bd commands via `CommandResult::ok = false`.
                let _ = handle.refresh_once().await;

                // Wait for the tick, or an explicit shutdown notification.
                let sleep = tokio::time::sleep(inner.interval);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {}
                    _ = inner.shutdown.notified() => {
                        break;
                    }
                }
            }
        })
    }

    /// Signal the polling task to exit. Uses `notify_one` so the permit is
    /// preserved if the task isn't yet awaiting the shutdown arm (e.g. because
    /// it's mid-refresh).
    pub fn shutdown(&self) {
        self.inner.shutdown.notify_one();
    }

    /// Cache a git diff payload (no TTL — evicted manually when memory gets
    /// tight). Key is `(repo_id, sha)`.
    pub fn cache_git_diff(&self, diff: CachedGitDiff) {
        let mut cache = self
            .inner
            .git_diff_cache
            .lock()
            .expect("git diff cache lock poisoned");
        cache.insert((diff.repo_id.clone(), diff.sha.clone()), diff);
    }

    /// Fetch a cached git diff if one exists.
    pub fn get_cached_git_diff(&self, repo_id: &str, sha: &str) -> Option<CachedGitDiff> {
        let cache = self
            .inner
            .git_diff_cache
            .lock()
            .expect("git diff cache lock poisoned");
        cache.get(&(repo_id.to_string(), sha.to_string())).cloned()
    }

    /// Clear the git diff cache — mostly a hook for future memory management.
    pub fn clear_git_diff_cache(&self) {
        self.inner
            .git_diff_cache
            .lock()
            .expect("git diff cache lock poisoned")
            .clear();
    }

    /// Expose a temporarily-borrowed Codex cache for callers in the sessions
    /// module. Returns a guard that releases the lock when dropped.
    pub fn with_codex_cache<R>(&self, f: impl FnOnce(&mut CodexCache) -> R) -> R {
        let mut cache = self
            .inner
            .codex_cache
            .lock()
            .expect("codex cache lock poisoned");
        f(&mut cache)
    }

    /// Same as [`SnapshotStore::with_codex_cache`] for the Claude cache.
    pub fn with_claude_cache<R>(&self, f: impl FnOnce(&mut ClaudeCache) -> R) -> R {
        let mut cache = self
            .inner
            .claude_cache
            .lock()
            .expect("claude cache lock poisoned");
        f(&mut cache)
    }

    /// Look up a repo's root path by id from the current snapshot.
    /// Mirrors `SnapshotStore.get_repo_root` in Python.
    pub fn get_repo_root(&self, repo_id: &str) -> Option<String> {
        let state = self
            .inner
            .state
            .lock()
            .expect("snapshot state lock poisoned");
        state
            .snapshot
            .git
            .as_object()
            .and_then(|obj| obj.get("repos"))
            .and_then(Value::as_array)
            .and_then(|repos| {
                repos
                    .iter()
                    .find(|r| r.get("id").and_then(Value::as_str) == Some(repo_id))
                    .cloned()
            })
            .and_then(|repo| repo.get("root").and_then(Value::as_str).map(String::from))
    }

    /// Look up a graph node (issue) by id. Returns a deep clone.
    pub fn get_node(&self, node_id: &str) -> Option<Value> {
        let state = self
            .inner
            .state
            .lock()
            .expect("snapshot state lock poisoned");
        state
            .snapshot
            .graph
            .as_object()
            .and_then(|obj| obj.get("nodes"))
            .and_then(Value::as_array)
            .and_then(|nodes| {
                nodes
                    .iter()
                    .find(|n| n.get("id").and_then(Value::as_str) == Some(node_id))
                    .cloned()
            })
    }

    /// Look up an agent by its target string (e.g. `gtui/polecats/nux`).
    pub fn get_agent(&self, target: &str) -> Option<AgentInfo> {
        let state = self
            .inner
            .state
            .lock()
            .expect("snapshot state lock poisoned");
        state
            .snapshot
            .agents
            .iter()
            .find(|a| a.target == target)
            .cloned()
    }

    /// Current tmux socket name as parsed from `gt status --fast`.
    pub fn get_tmux_socket(&self) -> String {
        self.inner
            .state
            .lock()
            .expect("snapshot state lock poisoned")
            .snapshot
            .status
            .tmux_socket
            .clone()
    }

    /// Current services list as parsed from `gt status --fast`.
    pub fn get_services(&self) -> Vec<String> {
        self.inner
            .state
            .lock()
            .expect("snapshot state lock poisoned")
            .snapshot
            .status
            .services
            .clone()
    }

    /// Port of `SnapshotStore.fetch_diff` — compute `git show` for a repo+sha
    /// and truncate to 500 lines. Returns a typed payload identical in JSON
    /// shape to the Python response.
    pub async fn fetch_diff(&self, repo_id: &str, sha: &str) -> Result<CachedGitDiff, String> {
        let repo_root = self
            .get_repo_root(repo_id)
            .ok_or_else(|| format!("Unknown repo id: {repo_id}"))?;
        let result = run_command(
            &[
                "git",
                "-C",
                &repo_root,
                "show",
                "--stat",
                "--patch",
                "--find-renames",
                "--format=fuller",
                "--no-ext-diff",
                sha,
            ],
            &self.inner.gt_root,
            RunOptions::default().with_timeout(Duration::from_secs(5)),
        )
        .await;
        if !result.ok {
            return Err(result.error);
        }
        let text = result
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        let truncated = if lines.len() > 500 {
            lines.truncate(500);
            lines.push(String::new());
            lines.push("[gtui] diff truncated to 500 lines".to_string());
            true
        } else {
            false
        };
        Ok(CachedGitDiff {
            repo_id: repo_id.to_string(),
            sha: sha.to_string(),
            text: lines.join("\n"),
            truncated,
        })
    }

    /// Send a `gt nudge` asking the target agent to pause. Mirrors
    /// `SnapshotStore.pause_agent` in Python. Returns the recorded action
    /// payload (also appended to the action history).
    pub async fn pause_agent(&self, target: &str) -> Result<Value, String> {
        let message = "Pause after your current step. Do not take new work or \
                       mutate state until further instruction from GTUI. Reply \
                       with a short status summary.";
        let command: [&str; 7] = [
            "gt",
            "nudge",
            target,
            "--mode",
            "wait-idle",
            "--message",
            message,
        ];
        let result = run_command(
            &command,
            &self.inner.gt_root,
            RunOptions::default().with_timeout(Duration::from_secs(4)),
        )
        .await;
        let action = json!({
            "kind": "pause-agent",
            "target": target,
            "command": display_command(&command),
            "ok": result.ok,
            "output": action_output(&result),
            "timestamp": now_iso(),
        });
        self.record_action(action.clone());
        let _ = self.refresh_once().await;
        Ok(action)
    }

    /// Send a free-form `gt nudge` to a target agent. Mirrors
    /// `SnapshotStore.inject_instruction` in Python.
    pub async fn inject_instruction(&self, target: &str, message: &str) -> Result<Value, String> {
        if message.trim().is_empty() {
            return Err("Instruction message is empty.".to_string());
        }
        let command: [&str; 7] = [
            "gt",
            "nudge",
            target,
            "--mode",
            "wait-idle",
            "--message",
            message,
        ];
        let result = run_command(
            &command,
            &self.inner.gt_root,
            RunOptions::default().with_timeout(Duration::from_secs(4)),
        )
        .await;
        let action = json!({
            "kind": "inject-instruction",
            "target": target,
            "command": display_command(&command),
            "ok": result.ok,
            "output": action_output(&result),
            "timestamp": now_iso(),
        });
        self.record_action(action.clone());
        let _ = self.refresh_once().await;
        Ok(action)
    }

    /// Port of `SnapshotStore.retry_task`. Uses the graph node's stored status
    /// to pick between `gt unsling` (hooked/running) and `gt release`
    /// (in_progress). Returns the recorded action payload.
    pub async fn retry_task(&self, task_id: &str) -> Result<Value, String> {
        let node = self
            .get_node(task_id)
            .ok_or_else(|| format!("Unknown task: {task_id}"))?;
        let status = node.get("status").and_then(Value::as_str).unwrap_or("");
        let ui_status = node.get("ui_status").and_then(Value::as_str).unwrap_or("");
        let command: Vec<String> = if status == "hooked" || ui_status == "running" {
            let target = node
                .get("agent_targets")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    format!("Task {task_id} is marked running but no hooked agent was found.")
                })?;
            vec![
                "gt".into(),
                "unsling".into(),
                task_id.into(),
                target.into(),
                "--force".into(),
            ]
        } else if status == "in_progress" {
            vec![
                "gt".into(),
                "release".into(),
                task_id.into(),
                "-r".into(),
                "GTUI retry requested".into(),
            ]
        } else {
            return Err(format!(
                "Task {task_id} is not in a retryable running state."
            ));
        };

        let result = run_command(
            &command,
            &self.inner.gt_root,
            RunOptions::default().with_timeout(Duration::from_secs(4)),
        )
        .await;
        let action = json!({
            "kind": "retry-task",
            "task_id": task_id,
            "command": display_command(&command),
            "ok": result.ok,
            "output": action_output(&result),
            "timestamp": now_iso(),
        });
        self.record_action(action.clone());
        let _ = self.refresh_once().await;
        Ok(action)
    }

    /// Port of `SnapshotStore.get_terminal_state`. Returns the agent snapshot
    /// plus a Claude transcript view when available, otherwise a captured pane
    /// transcript when tmux is reachable.
    pub async fn get_terminal_state(&self, target: &str) -> Result<Value, String> {
        let agent = self
            .get_agent(target)
            .ok_or_else(|| format!("Unknown terminal target: {target}"))?;

        let tmux_socket = self.get_tmux_socket();
        let pane_id = if !agent.pane_id.is_empty() {
            agent.pane_id.clone()
        } else {
            agent
                .hook
                .get("pane_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let tmux_target = if !pane_id.is_empty() {
            pane_id.clone()
        } else {
            agent.session_name.clone()
        };

        let mut log_lines: Vec<Value> = Vec::new();
        let mut capture_error = String::new();
        let (transcript_view, claude_view) = self.get_claude_view_for_agent(&agent);
        if !tmux_socket.is_empty()
            && !tmux_target.is_empty()
            && !transcript_view
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            let capture = run_command(
                &[
                    "tmux",
                    "-L",
                    tmux_socket.as_str(),
                    "capture-pane",
                    "-p",
                    "-t",
                    tmux_target.as_str(),
                    "-S",
                    "-240",
                ],
                &self.inner.gt_root,
                RunOptions::default().with_timeout(Duration::from_secs(1)),
            )
            .await;
            if capture.ok {
                let text = capture.data.as_ref().and_then(Value::as_str).unwrap_or("");
                log_lines = text
                    .lines()
                    .map(|line| Value::String(line.trim_end().to_string()))
                    .collect();
            } else {
                capture_error = capture.error;
            }
        }

        let services: Vec<Value> = self.get_services().into_iter().map(Value::String).collect();

        Ok(json!({
            "target": agent.target,
            "label": agent.label,
            "role": agent.role,
            "scope": agent.scope,
            "kind": agent.kind,
            "has_session": agent.has_session,
            "runtime_state": agent.runtime_state,
            "current_path": agent.current_path,
            "session_name": agent.session_name,
            "pane_id": pane_id,
            "current_command": agent.current_command,
            "hook": agent.hook,
            "events": agent.events,
            "log_lines": log_lines,
            "claude_view": claude_view,
            "transcript_view": transcript_view,
            "render_mode": if transcript_view.get("available").and_then(Value::as_bool).unwrap_or(false) { "claude" } else { "terminal" },
            "services": services,
            "capture_error": capture_error,
            "generated_at": now_iso(),
        }))
    }

    fn get_claude_view_for_agent(&self, agent: &AgentInfo) -> (Value, Value) {
        if agent.current_path.is_empty() {
            return (json!({}), json!({}));
        }
        let command = agent.current_command.to_ascii_lowercase();
        let looks_like_claude = command.is_empty()
            || command == "node"
            || command == "claude"
            || command == "claude.exe";
        if !looks_like_claude {
            return (json!({}), json!({}));
        }

        let claude_root = claude_projects_root();
        let view = self.with_claude_cache(|cache| {
            let session =
                find_claude_session(cache, &claude_root, &agent.current_path, Instant::now())?;
            let path_text = session.get("path").and_then(Value::as_str)?;
            let path = PathBuf::from(path_text);
            let mut view = parse_claude_transcript(cache, &path);
            if !view
                .get("available")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return None;
            }
            if let Some(obj) = view.as_object_mut() {
                obj.insert(
                    "cwd".into(),
                    Value::String(
                        session
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
                obj.insert(
                    "session_id".into(),
                    Value::String(
                        session
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
            }
            Some(view)
        });
        match view {
            Some(view) => (view.clone(), view),
            None => (json!({}), json!({})),
        }
    }

    /// Port of `SnapshotStore.write_terminal`. Sends keystrokes to the
    /// target's tmux pane. Returns the recorded action payload on success.
    pub async fn write_terminal(&self, target: &str, message: &str) -> Result<Value, String> {
        if message.trim().is_empty() {
            return Err("Terminal message is empty.".to_string());
        }
        let agent = self
            .get_agent(target)
            .ok_or_else(|| format!("Unknown terminal target: {target}"))?;
        if !agent.has_session {
            return Err(format!(
                "{target} does not currently have a live tmux session."
            ));
        }
        let tmux_socket = self.get_tmux_socket();
        let pane_id = if !agent.pane_id.is_empty() {
            agent.pane_id
        } else {
            agent
                .hook
                .get("pane_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let tmux_target = if !pane_id.is_empty() {
            pane_id
        } else {
            agent.session_name.clone()
        };
        if tmux_socket.is_empty() || tmux_target.is_empty() {
            return Err(format!(
                "No live tmux pane is known for {target}. Refresh the page and try again."
            ));
        }

        let mut last_command: Vec<String> = Vec::new();
        let mut last_error: Option<String> = None;
        let send_mode = terminal_send_mode(&agent.current_command);
        if matches!(
            send_mode,
            TerminalSendMode::CodexPaste | TerminalSendMode::ClaudePaste
        ) {
            let load_buffer = vec![
                "tmux".to_string(),
                "-L".to_string(),
                tmux_socket.clone(),
                "load-buffer".to_string(),
                "-".to_string(),
            ];
            last_command = load_buffer.clone();
            let load_result = run_command(
                &load_buffer,
                &self.inner.gt_root,
                RunOptions::default()
                    .with_timeout(Duration::from_secs(2))
                    .with_stdin(message),
            )
            .await;
            if !load_result.ok {
                last_error = Some(load_result.error);
            } else {
                let mut commands = vec![vec![
                    "tmux".to_string(),
                    "-L".to_string(),
                    tmux_socket.clone(),
                    "paste-buffer".to_string(),
                    "-d".to_string(),
                    "-p".to_string(),
                    "-t".to_string(),
                    tmux_target.clone(),
                ]];
                if send_mode == TerminalSendMode::CodexPaste {
                    commands.push(vec![
                        "tmux".to_string(),
                        "-L".to_string(),
                        tmux_socket.clone(),
                        "send-keys".to_string(),
                        "-t".to_string(),
                        tmux_target.clone(),
                        "Escape".to_string(),
                    ]);
                }
                commands.push(vec![
                    "tmux".to_string(),
                    "-L".to_string(),
                    tmux_socket.clone(),
                    "send-keys".to_string(),
                    "-t".to_string(),
                    tmux_target.clone(),
                    "Enter".to_string(),
                ]);

                for cmd in commands {
                    last_command = cmd.clone();
                    let r = run_command(
                        &cmd,
                        &self.inner.gt_root,
                        RunOptions::default().with_timeout(Duration::from_secs(2)),
                    )
                    .await;
                    if !r.ok {
                        last_error = Some(r.error);
                        break;
                    }
                }
            }
        } else {
            let lines: Vec<&str> = if message.contains('\n') {
                message.split('\n').collect()
            } else {
                vec![message]
            };
            for line in lines {
                if !line.is_empty() {
                    let cmd = vec![
                        "tmux".to_string(),
                        "-L".to_string(),
                        tmux_socket.clone(),
                        "send-keys".to_string(),
                        "-t".to_string(),
                        tmux_target.clone(),
                        "-l".to_string(),
                        line.to_string(),
                    ];
                    last_command = cmd.clone();
                    let r = run_command(
                        &cmd,
                        &self.inner.gt_root,
                        RunOptions::default().with_timeout(Duration::from_secs(2)),
                    )
                    .await;
                    if !r.ok {
                        last_error = Some(r.error);
                        break;
                    }
                }
                let enter = vec![
                    "tmux".to_string(),
                    "-L".to_string(),
                    tmux_socket.clone(),
                    "send-keys".to_string(),
                    "-t".to_string(),
                    tmux_target.clone(),
                    "Enter".to_string(),
                ];
                last_command = enter.clone();
                let r = run_command(
                    &enter,
                    &self.inner.gt_root,
                    RunOptions::default().with_timeout(Duration::from_secs(2)),
                )
                .await;
                if !r.ok {
                    last_error = Some(r.error);
                    break;
                }
            }
        }

        if let Some(err) = last_error {
            if err.contains("can't find window") || err.contains("can't find pane") {
                return Err(format!(
                    "{target} has a stale tmux pane reference. Refresh GTUI and try again."
                ));
            }
            return Err(err);
        }

        let action = json!({
            "kind": "write-terminal",
            "target": target,
            "command": display_command(&last_command),
            "ok": true,
            "output": format!("Sent to {target}"),
            "timestamp": now_iso(),
        });
        self.record_action(action.clone());
        Ok(action)
    }
}

/// Collapse a successful `CommandResult` payload into the `output` string
/// that action records surface. Failures surface the error text instead.
fn action_output(result: &crate::command::CommandResult) -> String {
    if result.ok {
        result
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        result.error.clone()
    }
}

/// Build one snapshot frame. Fans out the five common `gt` calls in parallel
/// and folds their errors into `snapshot.errors`. The downstream stubs
/// (agents / beads / git / convoys / graph) are placeholder `Value::Null`
/// shapes pending dedicated ports.
pub async fn build_snapshot(gt_root: &Path, action_history: &[Value]) -> WorkspaceSnapshot {
    let started = Instant::now();

    let (status_result, vitals_result, crew_list_result, crew_status_result, feed_result) = tokio::join!(
        run_command(&["gt", "status", "--fast"], gt_root, RunOptions::default(),),
        run_command(&["gt", "vitals"], gt_root, RunOptions::default()),
        run_command(
            &["gt", "crew", "list", "--all", "--json"],
            gt_root,
            RunOptions::default().parse_json(),
        ),
        run_command(
            &["gt", "crew", "status", "--json"],
            gt_root,
            RunOptions::default().parse_json(),
        ),
        run_command(
            &[
                "gt",
                "feed",
                "--plain",
                "--since",
                "20m",
                "--limit",
                "80",
                "--no-follow",
            ],
            gt_root,
            RunOptions::default(),
        ),
    );

    let mut errors: Vec<Value> = Vec::new();
    for result in [
        &status_result,
        &vitals_result,
        &crew_list_result,
        &crew_status_result,
        &feed_result,
    ] {
        if !result.ok {
            errors.push(result.to_error());
        }
    }

    let gt_commands_ms = status_result.duration_ms
        + vitals_result.duration_ms
        + crew_list_result.duration_ms
        + crew_status_result.duration_ms
        + feed_result.duration_ms;

    let status_summary: StatusSummary = if status_result.ok {
        let text = status_result
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("");
        parse_status_summary(text)
    } else {
        StatusSummary::default()
    };

    let vitals_raw = if vitals_result.ok {
        vitals_result
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        vitals_result.error.clone()
    };

    let crews_list: Vec<Value> = crew_list_result
        .data
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let crews_running: Vec<Value> = crew_status_result
        .data
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let crews = merge_crews(crews_list, crews_running);

    let feed_events: Vec<Value> = if feed_result.ok {
        let text = feed_result
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("");
        parse_feed(text)
    } else {
        Vec::new()
    };

    let (agents, agent_errors, agent_ms) = collect_agents(gt_root, &status_summary, &crews).await;
    errors.extend(agent_errors);

    // Downstream collectors are stubbed for this bead — future ports will
    // flesh these out. We keep the JSON shape the frontend already consumes.
    let graph = json!({
        "nodes": Vec::<Value>::new(),
        "edges": Vec::<Value>::new(),
    });
    let git = json!({
        "repos": Vec::<Value>::new(),
        "recent_commits": Vec::<Value>::new(),
        "task_memory": {},
    });
    let convoys = json!({
        "convoys": Vec::<Value>::new(),
        "task_index": {},
    });

    let alerts = derive_alerts(&status_summary, &crews);

    let summary = Metrics {
        active_agents: agents.iter().filter(|agent| agent.has_session).count() as u32,
        command_errors: errors.len() as u32,
        ..Metrics::default()
    };

    let actions_for_snapshot = action_history
        .iter()
        .take(SNAPSHOT_ACTION_LIMIT)
        .cloned()
        .collect();

    WorkspaceSnapshot {
        generated_at: now_iso(),
        generation_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        gt_root: gt_root.to_string_lossy().into_owned(),
        status: status_summary,
        vitals_raw,
        status_legend: default_status_legend(),
        summary,
        alerts,
        graph,
        activity: Activity::default(),
        git,
        convoys,
        crews,
        agents,
        stores: Vec::new(),
        actions: actions_for_snapshot,
        errors,
        timings: Timings {
            gt_commands_ms,
            agent_commands_ms: agent_ms,
            ..Timings::default()
        },
    }
    .tag_feed(feed_events)
}

async fn collect_agents(
    gt_root: &Path,
    status_summary: &StatusSummary,
    crews: &[Value],
) -> (Vec<AgentInfo>, Vec<Value>, u64) {
    let (mut agents, errors, duration_ms) =
        collect_tmux_agents(gt_root, &status_summary.tmux_socket).await;

    for crew in crews {
        let rig = crew.get("rig").and_then(Value::as_str).unwrap_or("");
        let name = crew.get("name").and_then(Value::as_str).unwrap_or("");
        if rig.is_empty() || name.is_empty() {
            continue;
        }
        let target = format!("{rig}/crew/{name}");
        let mut existing = agents
            .iter()
            .position(|agent| agent.target == target)
            .map(|idx| agents.remove(idx))
            .unwrap_or_default();
        existing.target = target.clone();
        existing.label = target;
        existing.role = "crew".into();
        existing.scope = rig.into();
        if existing.kind.is_empty() {
            existing.kind = "external".into();
        }
        if existing.current_path.is_empty() {
            existing.current_path = crew
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        existing.has_session = existing.has_session
            || crew
                .get("has_session")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        existing.crew = crew.clone();
        agents.push(existing);
    }

    agents.sort_by(|a, b| {
        (a.scope.as_str(), a.role.as_str(), a.target.as_str()).cmp(&(
            b.scope.as_str(),
            b.role.as_str(),
            b.target.as_str(),
        ))
    });
    (agents, errors, duration_ms)
}

async fn collect_tmux_agents(
    gt_root: &Path,
    tmux_socket: &str,
) -> (Vec<AgentInfo>, Vec<Value>, u64) {
    if tmux_socket.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }
    let result = run_command(
        &[
            "tmux",
            "-L",
            tmux_socket,
            "list-panes",
            "-a",
            "-F",
            "#{session_name}|#{window_name}|#{pane_id}|#{pane_current_path}|#{pane_current_command}",
        ],
        gt_root,
        RunOptions::default().with_timeout(Duration::from_secs(2)),
    )
    .await;
    let duration_ms = result.duration_ms;
    if !result.ok {
        return (Vec::new(), vec![result.to_error()], duration_ms);
    }

    let mut agents = Vec::new();
    let text = result.data.as_ref().and_then(Value::as_str).unwrap_or("");
    for line in text.lines() {
        let mut parts = line.splitn(5, '|');
        let session_name = parts.next().unwrap_or("").to_string();
        let _window_name = parts.next().unwrap_or("");
        let pane_id = parts.next().unwrap_or("").to_string();
        let pane_path = parts.next().unwrap_or("").to_string();
        let pane_command = parts.next().unwrap_or("").to_string();
        let Some(target) = parse_tmux_target(gt_root, &pane_path) else {
            continue;
        };
        if target.role == "boot" {
            continue;
        }
        agents.push(AgentInfo {
            target: target.target,
            label: target.label,
            role: target.role,
            scope: target.scope,
            kind: "tmux".into(),
            has_session: true,
            current_path: pane_path,
            session_name,
            pane_id,
            current_command: pane_command,
            ..AgentInfo::default()
        });
    }
    (agents, Vec::new(), duration_ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TmuxTarget {
    target: String,
    role: String,
    scope: String,
    label: String,
}

fn parse_tmux_target(gt_root: &Path, pane_path: &str) -> Option<TmuxTarget> {
    let root = std::fs::canonicalize(gt_root).ok()?;
    let path = std::fs::canonicalize(pane_path).ok()?;
    let relative = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return None;
    }

    if parts[0] == "mayor" {
        return Some(TmuxTarget {
            target: "mayor".into(),
            role: "mayor".into(),
            scope: "hq".into(),
            label: "mayor".into(),
        });
    }
    if parts[0] == "deacon" {
        if parts.len() >= 3 && parts[1] == "dogs" && parts[2] == "boot" {
            return Some(TmuxTarget {
                target: "boot".into(),
                role: "boot".into(),
                scope: "hq".into(),
                label: "boot".into(),
            });
        }
        return Some(TmuxTarget {
            target: "deacon".into(),
            role: "deacon".into(),
            scope: "hq".into(),
            label: "deacon".into(),
        });
    }
    if parts.len() >= 2 && parts[1] == "witness" {
        let rig = &parts[0];
        return Some(TmuxTarget {
            target: format!("{rig}/witness"),
            role: "witness".into(),
            scope: rig.clone(),
            label: format!("{rig}/witness"),
        });
    }
    if parts.len() >= 3 && parts[1] == "refinery" && parts[2] == "rig" {
        let rig = &parts[0];
        return Some(TmuxTarget {
            target: format!("{rig}/refinery"),
            role: "refinery".into(),
            scope: rig.clone(),
            label: format!("{rig}/refinery"),
        });
    }
    if parts.len() >= 3 && parts[1] == "polecats" {
        let rig = &parts[0];
        let name = &parts[2];
        return Some(TmuxTarget {
            target: format!("{rig}/polecats/{name}"),
            role: "polecat".into(),
            scope: rig.clone(),
            label: format!("{rig}/polecats/{name}"),
        });
    }
    if parts.len() >= 3 && parts[1] == "crew" {
        let rig = &parts[0];
        let name = &parts[2];
        return Some(TmuxTarget {
            target: format!("{rig}/crew/{name}"),
            role: "crew".into(),
            scope: rig.clone(),
            label: format!("{rig}/crew/{name}"),
        });
    }
    None
}

fn normalize_change_path(text: &str) -> String {
    let mut s = text.trim();
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s.to_string()
}

fn is_benign_crew_change(path: &str) -> bool {
    let text = normalize_change_path(path);
    if text.is_empty() {
        return false;
    }
    if matches!(text.as_str(), "gitignore" | ".gitignore" | ".beads") {
        return true;
    }
    text.starts_with(".beads/")
}

fn crew_path_list(crew: &Value, key: &str) -> Vec<String> {
    crew.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Port of `enrich_crew_workspace` from `webui/server.py`. Normalises
/// `git_modified` / `git_untracked`, partitions them into benign/risky paths
/// (benign = `.beads/*` or `.gitignore`), and attaches derived status fields
/// the UI consumes (`git_state`, `git_status_label`, `git_status_tone`,
/// `git_has_risky_changes`, `git_has_local_state_only`).
fn enrich_crew_workspace(crew: &mut Value) {
    let Some(_) = crew.as_object() else {
        return;
    };

    let modified = crew_path_list(crew, "git_modified");
    let untracked = crew_path_list(crew, "git_untracked");
    let (benign_modified, risky_modified): (Vec<String>, Vec<String>) = modified
        .iter()
        .cloned()
        .partition(|p| is_benign_crew_change(p));
    let (benign_untracked, risky_untracked): (Vec<String>, Vec<String>) = untracked
        .iter()
        .cloned()
        .partition(|p| is_benign_crew_change(p));

    let (git_state, git_status_label, git_status_tone) =
        if modified.is_empty() && untracked.is_empty() {
            ("clean", "git clean", "done")
        } else if !risky_modified.is_empty() || !risky_untracked.is_empty() {
            ("warning", "repo changes", "stuck")
        } else {
            ("local_state", "local state only", "memory")
        };

    let has_risky = !risky_modified.is_empty() || !risky_untracked.is_empty();
    let has_local_only = git_state == "local_state";

    let map = crew.as_object_mut().expect("object guarded above");
    map.insert("git_modified".into(), json!(modified));
    map.insert("git_untracked".into(), json!(untracked));
    map.insert("git_benign_modified".into(), json!(benign_modified));
    map.insert("git_benign_untracked".into(), json!(benign_untracked));
    map.insert("git_risky_modified".into(), json!(risky_modified));
    map.insert("git_risky_untracked".into(), json!(risky_untracked));
    map.insert("git_state".into(), json!(git_state));
    map.insert("git_status_label".into(), json!(git_status_label));
    map.insert("git_status_tone".into(), json!(git_status_tone));
    map.insert("git_has_risky_changes".into(), json!(has_risky));
    map.insert("git_has_local_state_only".into(), json!(has_local_only));
}

/// Port of `merge_crews` from `webui/server.py`. Combines the `gt crew list
/// --all --json` catalog with the `gt crew status --json` running-state feed,
/// keying by `(rig, name)` so the running row's branch/worktree/mail metadata
/// overlays the catalog entry. Each merged row is passed through
/// `enrich_crew_workspace` and the final list is sorted by `(rig, name)` for
/// stable UI ordering.
fn merge_crews(all_crews: Vec<Value>, running_crews: Vec<Value>) -> Vec<Value> {
    use std::collections::BTreeMap;

    fn key_of(v: &Value) -> (String, String) {
        let rig = v
            .get("rig")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        (rig, name)
    }

    let mut merged: BTreeMap<(String, String), Value> = BTreeMap::new();

    for item in all_crews {
        let key = key_of(&item);
        merged.insert(key, item);
    }

    for item in running_crews {
        let key = key_of(&item);
        let entry = merged.entry(key.clone()).or_insert_with(|| {
            json!({
                "rig": key.0,
                "name": key.1,
            })
        });
        if let (Some(base), Some(updates)) = (entry.as_object_mut(), item.as_object()) {
            for (k, v) in updates {
                base.insert(k.clone(), v.clone());
            }
        }
    }

    let mut crews: Vec<Value> = merged.into_values().collect();
    for crew in crews.iter_mut() {
        enrich_crew_workspace(crew);
    }
    // BTreeMap already iterated in (rig, name) order, but re-sorting keeps the
    // contract explicit for anyone who rearranges the map type later.
    crews.sort_by(|a, b| {
        let a_rig = a.get("rig").and_then(Value::as_str).unwrap_or("");
        let a_name = a.get("name").and_then(Value::as_str).unwrap_or("");
        let b_rig = b.get("rig").and_then(Value::as_str).unwrap_or("");
        let b_name = b.get("name").and_then(Value::as_str).unwrap_or("");
        (a_rig, a_name).cmp(&(b_rig, b_name))
    });
    crews
}

/// Alerts ported from the Python `build_snapshot` logic that are cheap without
/// needing the downstream collectors.
fn derive_alerts(status: &StatusSummary, crews: &[Value]) -> Vec<String> {
    let mut alerts = Vec::new();
    if status
        .services
        .iter()
        .any(|svc| svc.contains("daemon (stopped)"))
    {
        alerts.push("Gas Town daemon is stopped.".to_string());
    }
    let risky_crews = crews
        .iter()
        .filter(|c| {
            c.get("git_has_risky_changes")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    if risky_crews > 0 {
        alerts.push(format!(
            "{risky_crews} crew workspace(s) have risky repo changes."
        ));
    }
    alerts
}

/// A stable fingerprint of the snapshot used to decide whether downstream UI
/// needs to re-render. Uses a plain FNV-1a hash over the JSON encoding —
/// cheap, stable, and not cryptographic (which we don't need).
pub fn fingerprint_snapshot(snapshot: &WorkspaceSnapshot) -> u64 {
    // Skip the wall-clock fields that move every tick even when nothing has
    // actually changed.
    let mut clone = snapshot.clone();
    clone.generated_at.clear();
    clone.generation_ms = 0;
    clone.timings = Timings::default();
    // Error payloads carry `duration_ms` which drifts frame-to-frame even
    // when the underlying failure is identical. Strip those fields so
    // identical failures fingerprint the same.
    for entry in clone.errors.iter_mut() {
        if let Some(obj) = entry.as_object_mut() {
            obj.remove("duration_ms");
        }
    }
    match serde_json::to_vec(&clone) {
        Ok(bytes) => fnv1a(&bytes),
        Err(_) => 0,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

trait WorkspaceSnapshotFeedExt {
    fn tag_feed(self, feed_events: Vec<Value>) -> Self;
}

impl WorkspaceSnapshotFeedExt for WorkspaceSnapshot {
    /// Parked feed events on the `git.feed_events` slot for downstream
    /// collectors to consume. This keeps the events on the snapshot without
    /// needing to add a new field ahead of the dedicated port.
    fn tag_feed(mut self, feed_events: Vec<Value>) -> Self {
        if let Some(obj) = self.git.as_object_mut() {
            obj.insert("feed_events".into(), Value::Array(feed_events));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn action_history_respects_limit_and_order() {
        let store = SnapshotStore::new(tmp_root());
        for i in 0..30 {
            store.record_action(json!({"i": i}));
        }
        let history = store.action_history();
        assert_eq!(history.len(), ACTION_HISTORY_LIMIT);
        // Newest first.
        assert_eq!(history[0]["i"], 29);
        assert_eq!(
            history[ACTION_HISTORY_LIMIT - 1]["i"],
            30 - ACTION_HISTORY_LIMIT as i64
        );
    }

    #[test]
    fn git_diff_cache_round_trips() {
        let store = SnapshotStore::new(tmp_root());
        let entry = CachedGitDiff {
            repo_id: "repo1".into(),
            sha: "abc123".into(),
            text: "diff body".into(),
            truncated: false,
        };
        assert!(store.get_cached_git_diff("repo1", "abc123").is_none());
        store.cache_git_diff(entry.clone());
        assert_eq!(store.get_cached_git_diff("repo1", "abc123"), Some(entry));
        store.clear_git_diff_cache();
        assert!(store.get_cached_git_diff("repo1", "abc123").is_none());
    }

    #[test]
    fn fingerprint_ignores_wallclock_fields() {
        let a = WorkspaceSnapshot {
            generated_at: "2026-01-01T00:00:00Z".into(),
            generation_ms: 1,
            timings: Timings {
                gt_commands_ms: 5,
                ..Timings::default()
            },
            ..WorkspaceSnapshot::default()
        };
        let b = WorkspaceSnapshot {
            generated_at: "2030-04-21T12:00:00Z".into(),
            generation_ms: 999,
            timings: Timings {
                gt_commands_ms: 999,
                ..Timings::default()
            },
            ..WorkspaceSnapshot::default()
        };
        assert_eq!(fingerprint_snapshot(&a), fingerprint_snapshot(&b));
    }

    #[test]
    fn fingerprint_changes_when_alerts_change() {
        let base = WorkspaceSnapshot::default();
        let mut updated = base.clone();
        updated.alerts = vec!["fire".into()];
        assert_ne!(fingerprint_snapshot(&base), fingerprint_snapshot(&updated));
    }

    #[tokio::test]
    async fn refresh_once_installs_snapshot_and_returns_true_first_time() {
        let store = SnapshotStore::new(tmp_root());
        let before = store.get();
        assert_eq!(before.summary.command_errors, 0);
        // The `gt` binary is unlikely to exist in this test environment; we
        // assert the snapshot still installs and errors are recorded.
        assert!(store.refresh_once().await);
        let after = store.get();
        assert!(!after.gt_root.is_empty());
        // Five synthetic errors expected when gt is missing (five failed
        // commands) — but if the CI box *does* have gt installed we just
        // accept whatever shape came back.
        assert!(after.summary.command_errors <= 5);
    }

    #[test]
    fn install_snapshot_skips_identical_fingerprint() {
        // Directly drive the install path to avoid depending on the `gt`
        // binary's presence/timing in the test environment.
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            gt_root: "/tmp/a".into(),
            alerts: vec!["first frame".into()],
            ..WorkspaceSnapshot::default()
        };
        assert!(store.install_snapshot(snap.clone()), "first install writes");
        assert!(
            !store.install_snapshot(snap.clone()),
            "identical fingerprint must not re-install"
        );

        // Changing a user-visible field should re-install.
        let mut mutated = snap.clone();
        mutated.alerts.push("second alert".into());
        assert!(store.install_snapshot(mutated), "changed frame installs");
    }

    #[test]
    fn initial_snapshot_has_defaults() {
        let store = SnapshotStore::new("/tmp/store-test");
        let snap = store.get();
        assert_eq!(snap.gt_root, "/tmp/store-test");
        assert_eq!(snap.status_legend.len(), 7);
        assert_eq!(snap.alerts.len(), 0);
    }

    #[test]
    fn terminal_send_mode_uses_buffered_paste_for_agent_tuis() {
        assert_eq!(terminal_send_mode("codex"), TerminalSendMode::CodexPaste);
        assert_eq!(
            terminal_send_mode("/usr/local/bin/claude"),
            TerminalSendMode::ClaudePaste
        );
        assert_eq!(terminal_send_mode("node"), TerminalSendMode::ClaudePaste);
        assert_eq!(terminal_send_mode("zsh"), TerminalSendMode::LineKeys);
    }

    #[test]
    fn codex_and_claude_cache_handles_are_reachable() {
        let store = SnapshotStore::new(tmp_root());
        let codex_empty = store.with_codex_cache(|c| c.list.files.len());
        let claude_empty = store.with_claude_cache(|c| c.list.files.len());
        assert_eq!(codex_empty, 0);
        assert_eq!(claude_empty, 0);
    }

    #[tokio::test]
    async fn shutdown_stops_background_task() {
        let store = SnapshotStore::with_interval(tmp_root(), Duration::from_millis(25));
        let handle = store.spawn();
        // Give the loop a moment to actually refresh once.
        tokio::time::sleep(Duration::from_millis(75)).await;
        store.shutdown();
        // The task should exit promptly after shutdown.
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "polling task failed to stop on shutdown");
    }

    #[test]
    fn derive_alerts_reports_stopped_daemon() {
        let status = StatusSummary {
            services: vec!["daemon (stopped)".into(), "dolt (running)".into()],
            ..StatusSummary::default()
        };
        let alerts = derive_alerts(&status, &[]);
        assert!(alerts.iter().any(|a| a.contains("daemon is stopped")));
    }

    #[test]
    fn benign_crew_change_matches_beads_and_gitignore() {
        assert!(is_benign_crew_change(".beads"));
        assert!(is_benign_crew_change(".beads/cache.db"));
        assert!(is_benign_crew_change("./.beads/"));
        assert!(is_benign_crew_change(".gitignore"));
        assert!(is_benign_crew_change("gitignore"));
        assert!(!is_benign_crew_change(""));
        assert!(!is_benign_crew_change("src/main.rs"));
        assert!(!is_benign_crew_change(".beadsX"));
    }

    #[test]
    fn enrich_crew_workspace_marks_clean_workspace() {
        let mut crew = json!({"rig": "gtui", "name": "merv"});
        enrich_crew_workspace(&mut crew);
        assert_eq!(crew["git_state"], "clean");
        assert_eq!(crew["git_status_label"], "git clean");
        assert_eq!(crew["git_status_tone"], "done");
        assert_eq!(crew["git_has_risky_changes"], false);
        assert_eq!(crew["git_has_local_state_only"], false);
        assert_eq!(crew["git_modified"].as_array().unwrap().len(), 0);
        assert_eq!(crew["git_benign_modified"].as_array().unwrap().len(), 0);
        assert_eq!(crew["git_risky_modified"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn enrich_crew_workspace_partitions_benign_and_risky_changes() {
        let mut crew = json!({
            "rig": "gastown",
            "name": "merv",
            "git_modified": ["plugins/run.sh", ".gitignore"],
            "git_untracked": [".beads/feed.db", "README.md"],
        });
        enrich_crew_workspace(&mut crew);
        assert_eq!(crew["git_state"], "warning");
        assert_eq!(crew["git_status_label"], "repo changes");
        assert_eq!(crew["git_status_tone"], "stuck");
        assert_eq!(crew["git_has_risky_changes"], true);
        assert_eq!(crew["git_has_local_state_only"], false);
        assert_eq!(
            crew["git_benign_modified"],
            json!([".gitignore"]),
            "benign_modified should catch .gitignore"
        );
        assert_eq!(crew["git_risky_modified"], json!(["plugins/run.sh"]));
        assert_eq!(crew["git_benign_untracked"], json!([".beads/feed.db"]));
        assert_eq!(crew["git_risky_untracked"], json!(["README.md"]));
    }

    #[test]
    fn enrich_crew_workspace_reports_local_state_only_for_benign_changes() {
        let mut crew = json!({
            "rig": "gtui",
            "name": "merv",
            "git_modified": [".gitignore"],
            "git_untracked": [".beads/"],
        });
        enrich_crew_workspace(&mut crew);
        assert_eq!(crew["git_state"], "local_state");
        assert_eq!(crew["git_status_label"], "local state only");
        assert_eq!(crew["git_status_tone"], "memory");
        assert_eq!(crew["git_has_risky_changes"], false);
        assert_eq!(crew["git_has_local_state_only"], true);
    }

    #[test]
    fn enrich_crew_workspace_drops_empty_path_entries() {
        let mut crew = json!({
            "rig": "gtui",
            "name": "merv",
            "git_modified": ["", "src/lib.rs"],
            "git_untracked": [""],
        });
        enrich_crew_workspace(&mut crew);
        assert_eq!(crew["git_modified"], json!(["src/lib.rs"]));
        assert_eq!(crew["git_untracked"], json!(Vec::<String>::new()));
    }

    #[test]
    fn merge_crews_overlays_running_state_on_catalog() {
        let catalog = vec![json!({
            "rig": "gtui",
            "name": "merv",
            "path": "/gt/gtui/crew/merv",
        })];
        let running = vec![json!({
            "rig": "gtui",
            "name": "merv",
            "branch": "main",
            "has_session": true,
            "git_modified": ["src/main.rs"],
        })];
        let crews = merge_crews(catalog, running);
        assert_eq!(crews.len(), 1);
        assert_eq!(crews[0]["branch"], "main");
        assert_eq!(crews[0]["has_session"], true);
        assert_eq!(crews[0]["path"], "/gt/gtui/crew/merv");
        assert_eq!(crews[0]["git_has_risky_changes"], true);
        assert_eq!(crews[0]["git_risky_modified"], json!(["src/main.rs"]));
    }

    #[test]
    fn merge_crews_keeps_catalog_only_rows_and_sorts_by_rig_then_name() {
        let catalog = vec![
            json!({"rig": "zeta", "name": "b"}),
            json!({"rig": "alpha", "name": "b"}),
            json!({"rig": "alpha", "name": "a"}),
        ];
        let running = vec![];
        let crews = merge_crews(catalog, running);
        assert_eq!(crews.len(), 3);
        assert_eq!(crews[0]["rig"], "alpha");
        assert_eq!(crews[0]["name"], "a");
        assert_eq!(crews[1]["rig"], "alpha");
        assert_eq!(crews[1]["name"], "b");
        assert_eq!(crews[2]["rig"], "zeta");
        // Enrichment always runs, even on sparse catalog rows.
        assert_eq!(crews[0]["git_state"], "clean");
    }

    #[test]
    fn merge_crews_creates_rows_from_running_only() {
        let catalog = vec![];
        let running = vec![json!({
            "rig": "gtui",
            "name": "ephemeral",
            "branch": "feature/x",
        })];
        let crews = merge_crews(catalog, running);
        assert_eq!(crews.len(), 1);
        assert_eq!(crews[0]["rig"], "gtui");
        assert_eq!(crews[0]["name"], "ephemeral");
        assert_eq!(crews[0]["branch"], "feature/x");
        assert_eq!(crews[0]["git_has_risky_changes"], false);
    }

    #[test]
    fn derive_alerts_counts_risky_crews() {
        let crews = vec![
            json!({"name": "a", "git_has_risky_changes": true}),
            json!({"name": "b"}),
            json!({"name": "c", "git_has_risky_changes": true}),
        ];
        let alerts = derive_alerts(&StatusSummary::default(), &crews);
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains('2'));
    }
}
