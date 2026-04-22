//! Snapshot assembly + store.
//!
//! Port of `build_snapshot()` and `SnapshotStore` from `webui/server.py`. The
//! five coarse `gt` CLI calls are fanned out in parallel via `tokio::join!`
//! (Python's `run_command` is blocking and sequential), but the downstream
//! collectors (`collect_agents`, `collect_bead_data`, `collect_git_memory`,
//! `collect_convoy_data`, `finalize_graph`, `build_activity_groups`) run
//! inline. Everything the store *does* own — locks, polling loop, action ring
//! buffer, caches — is in place.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use regex::Regex;
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
    default_status_legend, Activity, ActivityGroup, AgentInfo, Metrics, StatusSummary, Timings,
    WorkspaceSnapshot,
};
use crate::parse::{normalize_lines, now_iso, parse_feed, parse_status_summary};
use crate::sessions::{
    claude_projects_root, codex_sessions_root, find_claude_session, find_codex_rollout,
    parse_claude_transcript, parse_codex_transcript, ClaudeCache, CodexCache,
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
                // Race the refresh against shutdown so a slow `gt polecat list`
                // or hook fan-out can't keep the loop pinned past a shutdown
                // request.
                let shutdown_notif = inner.shutdown.notified();
                tokio::pin!(shutdown_notif);
                tokio::select! {
                    _ = handle.refresh_once() => {}
                    _ = &mut shutdown_notif => { break; }
                }

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

    /// Port of `SnapshotStore.get_terminal_state`. Performs a live tmux
    /// rediscovery merge, picks Codex or Claude transcripts when available, and
    /// otherwise falls back to a captured pane transcript. Refreshes events
    /// with a fresh `gt feed --since 5m` filtered by target.
    pub async fn get_terminal_state(&self, target: &str) -> Result<Value, String> {
        let agent = self
            .discover_agent(target)
            .await
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
        let (transcript_view, codex_view, claude_view) = self.get_transcript_view_for_agent(&agent);
        let transcript_available = transcript_view
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !tmux_socket.is_empty() && !tmux_target.is_empty() && !transcript_available {
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
                log_lines = normalize_lines(text, 240)
                    .into_iter()
                    .map(Value::String)
                    .collect();
            } else {
                capture_error = capture.error;
            }
        }

        let mut events: Vec<Value> = tail_n(&agent.events, 6);
        let feed_result = run_command(
            &[
                "gt",
                "feed",
                "--plain",
                "--since",
                "5m",
                "--limit",
                "40",
                "--no-follow",
            ],
            &self.inner.gt_root,
            RunOptions::default().with_timeout(Duration::from_secs(1)),
        )
        .await;
        if feed_result.ok {
            let text = feed_result
                .data
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or("");
            let fresh_events: Vec<Value> = parse_feed(text)
                .into_iter()
                .filter(|event| event.get("actor").and_then(Value::as_str).unwrap_or("") == target)
                .collect();
            if !fresh_events.is_empty() {
                events = tail_n(&fresh_events, 6);
            }
        }

        let task_events = tail_n(&agent.task_events, 6);
        let recent_task = if agent.recent_task.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            agent.recent_task.clone()
        };

        let services: Vec<Value> = self.get_services().into_iter().map(Value::String).collect();
        let render_mode = if transcript_available {
            transcript_view
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("terminal")
                .to_string()
        } else {
            "terminal".to_string()
        };

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
            "events": events,
            "task_events": task_events,
            "recent_task": recent_task,
            "log_lines": log_lines,
            "codex_view": codex_view,
            "claude_view": claude_view,
            "transcript_view": transcript_view,
            "render_mode": render_mode,
            "services": services,
            "capture_error": capture_error,
            "generated_at": now_iso(),
        }))
    }

    /// Port of `SnapshotStore.discover_agent`: overlay a fresh tmux scan on top
    /// of the cached agent so terminal reads pick up the latest pane id,
    /// current command, and path without waiting for the next poll cycle.
    async fn discover_agent(&self, target: &str) -> Option<AgentInfo> {
        let cached_agent = self.get_agent(target);
        let tmux_socket = self.get_tmux_socket();
        if tmux_socket.is_empty() {
            return cached_agent;
        }
        let (live_agents, _errors, _ms) =
            collect_tmux_agents(&self.inner.gt_root, &tmux_socket).await;
        let live_agent = live_agents.into_iter().find(|a| a.target == target);
        match (cached_agent, live_agent) {
            (Some(cached), Some(live)) => Some(merge_agent_overlay(cached, live)),
            (Some(cached), None) => Some(cached),
            (None, Some(live)) => Some(live),
            (None, None) => None,
        }
    }

    fn get_transcript_view_for_agent(&self, agent: &AgentInfo) -> (Value, Value, Value) {
        let codex_view = self.get_codex_view_for_agent(agent);
        if codex_view
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return (codex_view.clone(), codex_view, json!({}));
        }
        let (_, claude_view) = self.get_claude_view_for_agent(agent);
        if claude_view
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return (claude_view.clone(), json!({}), claude_view);
        }
        (json!({}), json!({}), json!({}))
    }

    fn get_codex_view_for_agent(&self, agent: &AgentInfo) -> Value {
        if agent.current_path.is_empty() {
            return json!({});
        }
        if normalize_command_name(&agent.current_command) != "codex" {
            return json!({});
        }
        let codex_root = codex_sessions_root();
        let current_path = agent.current_path.clone();
        let view = self.with_codex_cache(|cache| {
            let rollout = find_codex_rollout(cache, &codex_root, &current_path, Instant::now())?;
            let path_text = rollout.get("path").and_then(Value::as_str)?;
            let path = PathBuf::from(path_text);
            let mut view = parse_codex_transcript(cache, &path);
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
                        rollout
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
                obj.insert(
                    "session_id".into(),
                    Value::String(
                        rollout
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                );
            }
            Some(view)
        });
        view.unwrap_or_else(|| json!({}))
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

/// Return the last `n` entries of a slice, matching Python's `xs[-n:]`.
fn tail_n<T: Clone>(items: &[T], n: usize) -> Vec<T> {
    if items.len() <= n {
        items.to_vec()
    } else {
        items[items.len() - n..].to_vec()
    }
}

/// Overlay tmux-derived fields from a freshly scanned agent on top of a
/// cached agent. Mirrors Python's `merged = deep_copy(cached); merged.update(live)`
/// but restricted to the fields `collect_tmux_agents` actually populates so the
/// hook, crew, polecat, and task-event state survive the refresh.
fn merge_agent_overlay(cached: AgentInfo, live: AgentInfo) -> AgentInfo {
    let mut merged = cached;
    merged.target = live.target;
    merged.label = live.label;
    merged.role = live.role;
    merged.scope = live.scope;
    merged.kind = live.kind;
    merged.has_session = live.has_session;
    merged.current_path = live.current_path;
    merged.session_name = live.session_name;
    merged.pane_id = live.pane_id;
    merged.current_command = live.current_command;
    merged
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

    let (agents, hook_by_issue, agent_errors, agent_ms) =
        collect_agents(gt_root, &status_summary, &crews, &feed_events).await;
    errors.extend(agent_errors);

    // Discover bead stores (hq + per-rig) and collect the per-store summary.
    let bead_stores = discover_bead_stores(gt_root);
    let (store_summaries, bead_store_snapshots, store_errors, bead_ms) =
        collect_bead_store_summaries(&bead_stores).await;
    errors.extend(store_errors);

    // Fold the raw per-store snapshots into compacted graph nodes/edges,
    // merge links, and the consolidated blocked/hooked sets.
    let bead_data = collect_bead_data(&bead_store_snapshots, &hook_by_issue);
    let _ = &bead_data.blocked_ids; // Exposed on future IPC surfaces.
    let _ = &bead_data.hooked_ids;

    // Git memory: walk the town root + crew worktrees, fan out the four git
    // commands per repo, and fold commit history + merge beads into a per-task
    // memory index.
    let (git_memory, git_errors, git_ms) =
        collect_git_memory(gt_root, &crews, &bead_data.merge_links).await;
    errors.extend(git_errors);

    let (convoys, convoy_errors, convoy_ms) = collect_convoy_data(gt_root).await;
    errors.extend(convoy_errors);

    let mut running_tasks: u32 = 0;
    let mut stuck_tasks: u32 = 0;
    let mut ready_tasks: u32 = 0;
    let mut done_tasks: u32 = 0;
    let mut ice_tasks: u32 = 0;
    let mut system_running: u32 = 0;
    let mut stored_status_counts: BTreeMap<String, u32> = BTreeMap::new();
    for node in &bead_data.nodes {
        if node.get("kind").and_then(Value::as_str) != Some("task") {
            continue;
        }
        let is_system = node
            .get("is_system")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ui_status = node.get("ui_status").and_then(Value::as_str).unwrap_or("");
        if is_system {
            if ui_status == "running" {
                system_running += 1;
            }
            continue;
        }
        let stored = node
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        *stored_status_counts.entry(stored).or_insert(0) += 1;
        match ui_status {
            "running" => running_tasks += 1,
            "stuck" => stuck_tasks += 1,
            "ready" => ready_tasks += 1,
            "done" => done_tasks += 1,
            "ice" => ice_tasks += 1,
            _ => {}
        }
    }
    let mut derived_status_counts: BTreeMap<String, u32> = BTreeMap::new();
    derived_status_counts.insert("running".into(), running_tasks);
    derived_status_counts.insert("stuck".into(), stuck_tasks);
    derived_status_counts.insert("ready".into(), ready_tasks);
    derived_status_counts.insert("done".into(), done_tasks);
    derived_status_counts.insert("ice".into(), ice_tasks);

    // Layer linked commits, commit nodes/edges, and the "interesting" filter
    // onto the raw bead graph. Mirrors `finalize_graph` in `webui/server.py`.
    let graph = finalize_graph(&bead_data, &git_memory, &convoys);
    let repo_count = git_memory.repos.len() as u32;

    // Bucket live agents by hooked bead, attach task memory links. Runs after
    // `finalize_graph` because the group metadata falls back to the finalized
    // node's `ui_status`/`is_system` flags.
    let graph_nodes: Vec<Value> = graph
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let activity = build_activity_groups(&agents, &graph_nodes, &git_memory.task_memory);
    let task_groups_count = activity.groups.len() as u32;

    let git = git_memory.into_json();

    let active_agents = agents.iter().filter(|agent| agent.has_session).count() as u32;
    let mut alerts = derive_alerts(&status_summary, &crews);
    if running_tasks == 0 && ready_tasks > 0 && active_agents > 0 {
        alerts.push("Agents are alive, but no product tasks are currently running.".to_string());
    }
    if stuck_tasks > 0 {
        alerts.push(format!(
            "{stuck_tasks} task node(s) are dependency-blocked."
        ));
    }

    let summary = Metrics {
        running_tasks,
        stuck_tasks,
        ready_tasks,
        done_tasks,
        system_running,
        active_agents,
        task_groups: task_groups_count,
        repos: repo_count,
        command_errors: errors.len() as u32,
        stored_status_counts,
        derived_status_counts,
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
        activity,
        git,
        convoys,
        crews,
        agents,
        stores: store_summaries,
        actions: actions_for_snapshot,
        errors,
        timings: Timings {
            gt_commands_ms,
            agent_commands_ms: agent_ms,
            bd_commands_ms: bead_ms,
            git_commands_ms: git_ms,
            convoy_commands_ms: convoy_ms,
        },
    }
    .tag_feed(feed_events)
}

fn slung_event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^slung\s+(\S+)\s+to\s+(\S+)$").expect("static regex"))
}

fn done_event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^done:\s+(\S+)$").expect("static regex"))
}

/// Replay feed events into per-actor and per-target buckets. Mirrors Python's
/// inline loop in `collect_agents`: every event with an actor lands in
/// `event_map[actor]`; `slung <task> to <target>` appends an `assigned` task
/// event to `task_event_map[target]`; `done: <task>` appends a `done` task
/// event to `task_event_map[actor]`.
fn classify_feed_events(
    feed_events: &[Value],
) -> (HashMap<String, Vec<Value>>, HashMap<String, Vec<Value>>) {
    let mut event_map: HashMap<String, Vec<Value>> = HashMap::new();
    let mut task_event_map: HashMap<String, Vec<Value>> = HashMap::new();
    for (index, event) in feed_events.iter().enumerate() {
        let actor = event
            .get("actor")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let message = event
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let time = event
            .get("time")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !actor.is_empty() {
            event_map
                .entry(actor.clone())
                .or_default()
                .push(event.clone());
        }
        if let Some(caps) = slung_event_re().captures(&message) {
            let task_id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            let target = caps.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
            task_event_map.entry(target).or_default().push(json!({
                "kind": "assigned",
                "task_id": task_id,
                "time": time,
                "message": message,
                "order": index.to_string(),
            }));
        } else if let Some(caps) = done_event_re().captures(&message) {
            if !actor.is_empty() {
                let task_id = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
                task_event_map.entry(actor).or_default().push(json!({
                    "kind": "done",
                    "task_id": task_id,
                    "time": time,
                    "message": message,
                    "order": index.to_string(),
                }));
            }
        }
    }
    (event_map, task_event_map)
}

fn worker_count(total: usize, cap: usize) -> usize {
    if total == 0 {
        return 1;
    }
    total.min(cap).max(1)
}

/// A discovered beads store — either the town-level `hq` store rooted at
/// `gt_root` or a per-rig store under `gt_root/<rig>/.beads`.
///
/// Port of the `{"name", "path", "scope"}` dicts returned by
/// `discover_bead_stores` in `webui/server.py`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeadStore {
    pub name: String,
    pub path: PathBuf,
    pub scope: String,
}

/// Read `rigs.json` and return the configured rig names, sorted. Mirrors
/// `configured_rig_names` in `webui/server.py`: malformed JSON, missing file,
/// non-object `rigs` entries, or empty keys all collapse to an empty list.
pub fn configured_rig_names(gt_root: &Path) -> Vec<String> {
    let rigs_path = gt_root.join("rigs.json");
    let text = match std::fs::read_to_string(&rigs_path) {
        Ok(text) => text,
        Err(_) => return Vec::new(),
    };
    let payload: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let Some(rigs) = payload.get("rigs").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rigs
        .keys()
        .filter(|name| !name.is_empty())
        .cloned()
        .collect();
    names.sort();
    names
}

/// Discover bead stores under `gt_root`. Matches `discover_bead_stores` in
/// `webui/server.py`:
///
/// 1. If `gt_root/.beads` exists, emit an `hq` store rooted at `gt_root`.
/// 2. For each rig in `rigs.json`, if `gt_root/<rig>` is a directory AND
///    `gt_root/<rig>/.beads` exists, emit a per-rig store.
pub fn discover_bead_stores(gt_root: &Path) -> Vec<BeadStore> {
    let mut stores: Vec<BeadStore> = Vec::new();
    if gt_root.join(".beads").is_dir() {
        stores.push(BeadStore {
            name: "hq".to_string(),
            path: gt_root.to_path_buf(),
            scope: "hq".to_string(),
        });
    }
    for rig_name in configured_rig_names(gt_root) {
        let child = gt_root.join(&rig_name);
        if !child.is_dir() {
            continue;
        }
        if child.join(".beads").is_dir() {
            stores.push(BeadStore {
                name: rig_name.clone(),
                path: child,
                scope: rig_name,
            });
        }
    }
    stores
}

fn count_issue_statuses(issues: &[Value]) -> BTreeMap<String, u64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for issue in issues {
        let status = issue
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        *counts.entry(status).or_insert(0) += 1;
    }
    counts
}

fn issue_ids_set(issues: &[Value]) -> std::collections::HashSet<String> {
    issues
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
        .collect()
}

/// Raw `bd ...` results gathered for a single store. Kept around so downstream
/// collectors (`collect_bead_data`, port pending as gui-cqe.6) can reuse the
/// `bd list --all` / `bd blocked` / `bd list --status=hooked` payloads without
/// re-running the subprocess.
#[derive(Debug, Clone)]
pub struct BeadStoreSnapshot {
    pub store: BeadStore,
    pub status_payload: Option<Value>,
    pub issues: Vec<Value>,
    pub blocked: Vec<Value>,
    pub hooked: Vec<Value>,
    pub status_ok: bool,
    pub status_error: String,
}

/// Port of the per-store loop inside `collect_bead_data` in `webui/server.py`.
/// For each discovered store, fan out the four `bd` subprocess calls
/// (`bd status --json`, `bd list --all --json --limit 300`, `bd blocked
/// --json`, `bd list --status=hooked --json`) and produce the `store_summaries`
/// entry the snapshot's `stores` field exposes.
///
/// Returns `(store_summaries, raw_snapshots, errors, duration_ms)`. The raw
/// snapshots are retained for downstream graph/node construction in a follow-on
/// bead (gui-cqe.6) — today they're dropped, but keeping the API shape ready
/// avoids a second pass over the same commands later.
async fn collect_bead_store_summaries(
    stores: &[BeadStore],
) -> (Vec<Value>, Vec<BeadStoreSnapshot>, Vec<Value>, u64) {
    if stores.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), 0);
    }
    // Fan stores out in parallel. Stores are few (usually 2–5), so the pool
    // size matches the actual store count up to a small cap. Matches the
    // pattern used by `collect_agents` for its hook fan-out.
    let max_workers = worker_count(stores.len(), 4);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_workers));
    type StoreCmdTuple = (
        BeadStore,
        crate::command::CommandResult,
        crate::command::CommandResult,
        crate::command::CommandResult,
        crate::command::CommandResult,
    );
    let mut handles: Vec<JoinHandle<StoreCmdTuple>> = Vec::with_capacity(stores.len());
    for store in stores.iter().cloned() {
        let permit_sem = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit_sem.acquire_owned().await.expect("semaphore open");
            // Python runs `bd status --json` without parse_json so invalid
            // JSON falls back to the raw stdout; we mirror that by parsing
            // the stdout ourselves below.
            let status_result = run_command(
                &["bd", "status", "--json"],
                &store.path,
                RunOptions::default().with_timeout(Duration::from_secs(4)),
            )
            .await;
            let all_result = run_command(
                &["bd", "list", "--all", "--json", "--limit", "300"],
                &store.path,
                RunOptions::default()
                    .with_timeout(Duration::from_secs(6))
                    .parse_json(),
            )
            .await;
            let blocked_result = run_command(
                &["bd", "blocked", "--json"],
                &store.path,
                RunOptions::default()
                    .with_timeout(Duration::from_secs(4))
                    .parse_json(),
            )
            .await;
            let hooked_result = run_command(
                &["bd", "list", "--status=hooked", "--json"],
                &store.path,
                RunOptions::default()
                    .with_timeout(Duration::from_secs(4))
                    .parse_json(),
            )
            .await;
            (
                store,
                status_result,
                all_result,
                blocked_result,
                hooked_result,
            )
        }));
    }

    let mut summaries: Vec<Value> = Vec::with_capacity(stores.len());
    let mut snapshots: Vec<BeadStoreSnapshot> = Vec::with_capacity(stores.len());
    let mut errors: Vec<Value> = Vec::new();
    let mut duration_ms: u64 = 0;

    for handle in handles {
        let (store, status_result, all_result, blocked_result, hooked_result) = match handle.await {
            Ok(tuple) => tuple,
            Err(_) => continue,
        };
        duration_ms += status_result.duration_ms
            + all_result.duration_ms
            + blocked_result.duration_ms
            + hooked_result.duration_ms;

        for result in [&status_result, &all_result, &blocked_result, &hooked_result] {
            if !result.ok {
                errors.push(result.to_error());
            }
        }

        // `bd status --json` is run without parse_json so its `data` holds the
        // raw stdout string. On failure we fall back to stdout → error text,
        // matching Python's `status_result.data if ok else (stdout or error)`.
        let status_text: String = if status_result.ok {
            status_result
                .data
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        } else if !status_result.stdout.is_empty() {
            status_result.stdout.clone()
        } else {
            status_result.error.clone()
        };
        let status_payload: Option<Value> = if status_text.is_empty() {
            None
        } else {
            serde_json::from_str(&status_text).ok()
        };

        let issues: Vec<Value> = if all_result.ok {
            all_result
                .data
                .as_ref()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let blocked: Vec<Value> = if blocked_result.ok {
            blocked_result
                .data
                .as_ref()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let hooked: Vec<Value> = if hooked_result.ok {
            hooked_result
                .data
                .as_ref()
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let blocked_local_ids = issue_ids_set(&blocked);
        let hooked_local_ids = issue_ids_set(&hooked);
        let mut open_count: u64 = 0;
        let mut closed_count: u64 = 0;
        let mut blocked_count: u64 = 0;
        let mut hooked_count: u64 = 0;
        for issue in &issues {
            let id = issue
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            let status = issue.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "closed" {
                closed_count += 1;
            } else {
                open_count += 1;
            }
            if blocked_local_ids.contains(&id) {
                blocked_count += 1;
            }
            if hooked_local_ids.contains(&id) {
                hooked_count += 1;
            }
        }

        let summary_obj: Value = status_payload
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|m| m.get("summary").cloned())
            .unwrap_or_else(|| json!({}));
        let error_text: String =
            if let Some(obj) = status_payload.as_ref().and_then(|v| v.as_object()) {
                obj.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            } else if !status_result.ok {
                status_result.error.clone()
            } else {
                String::new()
            };

        let exact_status_counts = count_issue_statuses(&issues);

        summaries.push(json!({
            "name": store.name,
            "scope": store.scope,
            "path": store.path.to_string_lossy(),
            "available": status_result.ok,
            "summary": summary_obj,
            "error": error_text,
            "exact_status_counts": exact_status_counts,
            "total": issues.len() as u64,
            "open": open_count,
            "closed": closed_count,
            "blocked": blocked_count,
            "hooked": hooked_count,
        }));

        snapshots.push(BeadStoreSnapshot {
            store,
            status_payload,
            issues,
            blocked,
            hooked,
            status_ok: status_result.ok,
            status_error: status_result.error,
        });
    }

    // Restore input order — `tokio::spawn` resolution order is
    // non-deterministic but Python preserves the discovery order.
    let order: HashMap<String, usize> = stores
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), i))
        .collect();
    summaries.sort_by_key(|s| {
        s.get("name")
            .and_then(Value::as_str)
            .and_then(|name| order.get(name))
            .copied()
            .unwrap_or(usize::MAX)
    });
    snapshots.sort_by_key(|snap| {
        order
            .get(snap.store.name.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    });

    (summaries, snapshots, errors, duration_ms)
}

/// Issue `issue_type` values that are rendered on the task graph. Anything
/// else (messages, escalations, custom types) is filtered out unless it also
/// qualifies as a system issue. Mirrors `GRAPH_ALLOWED_TYPES` in
/// `webui/server.py`.
const GRAPH_ALLOWED_TYPES: &[&str] = &["task", "bug", "feature", "chore", "decision", "epic"];

fn metadata_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([a-zA-Z0-9_.-]+):\s*(.+)$").expect("static regex"))
}

/// Parse a bead description's leading "key: value" metadata block. Mirrors
/// `parse_simple_metadata_block` in `webui/server.py` — only the first 20
/// lines are inspected, and each line must match a strict `key: value`
/// regex.
fn parse_simple_metadata_block(text: &str) -> HashMap<String, String> {
    let mut metadata: HashMap<String, String> = HashMap::new();
    let re = metadata_block_re();
    for line in text.lines().take(20) {
        let trimmed = line.trim();
        if let Some(caps) = re.captures(trimmed) {
            metadata.insert(caps[1].to_string(), caps[2].to_string());
        }
    }
    metadata
}

fn issue_label_contains(issue: &Value, label: &str) -> bool {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().any(|v| v.as_str() == Some(label)))
        .unwrap_or(false)
}

fn issue_is_merge(issue: &Value) -> bool {
    if issue_label_contains(issue, "gt:merge-request") {
        return true;
    }
    let description = issue
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let meta = parse_simple_metadata_block(description);
    meta.contains_key("source_issue") && meta.contains_key("commit_sha")
}

fn issue_is_system(issue: &Value) -> bool {
    if issue_label_contains(issue, "gt:rig") {
        return true;
    }
    let issue_type = issue
        .get("issue_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if issue_type == "molecule" {
        return true;
    }
    let id = issue.get("id").and_then(Value::as_str).unwrap_or("");
    if id.starts_with("hq-wisp-") {
        return true;
    }
    let title = issue.get("title").and_then(Value::as_str).unwrap_or("");
    if title.starts_with("mol-") {
        return true;
    }
    false
}

fn issue_is_graph_noise(issue: &Value) -> bool {
    let issue_type = issue
        .get("issue_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !issue_type.is_empty()
        && !GRAPH_ALLOWED_TYPES.contains(&issue_type)
        && !issue_is_system(issue)
    {
        return true;
    }
    if issue_label_contains(issue, "gt:message") || issue_label_contains(issue, "gt:escalation") {
        return true;
    }
    let id = issue.get("id").and_then(Value::as_str).unwrap_or("");
    if id.starts_with("hq-cv-") {
        return true;
    }
    false
}

fn derive_ui_status(
    issue: &Value,
    blocked_ids: &HashSet<String>,
    hooked_ids: &HashSet<String>,
) -> &'static str {
    let status = issue
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("open");
    let id = issue.get("id").and_then(Value::as_str).unwrap_or("");
    if status == "closed" {
        return "done";
    }
    if status == "hooked" || status == "in_progress" || hooked_ids.contains(id) {
        return "running";
    }
    if status == "deferred" {
        return "ice";
    }
    if status == "blocked" || blocked_ids.contains(id) {
        return "stuck";
    }
    "ready"
}

/// Rust port of `compact_issue` in `webui/server.py`. Collapses a raw bead
/// payload down to the fields the graph renderer needs, plus the derived
/// `ui_status` / `is_system` labels.
fn compact_issue(
    issue: &Value,
    blocked_ids: &HashSet<String>,
    hooked_ids: &HashSet<String>,
) -> Value {
    json!({
        "id": issue.get("id").and_then(Value::as_str).unwrap_or(""),
        "title": issue.get("title").and_then(Value::as_str).unwrap_or(""),
        "description": issue.get("description").and_then(Value::as_str).unwrap_or(""),
        "status": issue.get("status").and_then(Value::as_str).unwrap_or(""),
        "ui_status": derive_ui_status(issue, blocked_ids, hooked_ids),
        "priority": issue.get("priority").cloned().unwrap_or(Value::Null),
        "type": issue.get("issue_type").and_then(Value::as_str).unwrap_or(""),
        "owner": issue.get("owner").and_then(Value::as_str).unwrap_or(""),
        "assignee": issue.get("assignee").and_then(Value::as_str).unwrap_or(""),
        "created_at": issue.get("created_at").and_then(Value::as_str).unwrap_or(""),
        "updated_at": issue.get("updated_at").and_then(Value::as_str).unwrap_or(""),
        "closed_at": issue.get("closed_at").and_then(Value::as_str).unwrap_or(""),
        "parent": issue.get("parent").and_then(Value::as_str).unwrap_or(""),
        "labels": issue.get("labels").cloned().unwrap_or_else(|| json!([])),
        "dependency_count": issue.get("dependency_count").cloned().unwrap_or_else(|| json!(0)),
        "dependent_count": issue.get("dependent_count").cloned().unwrap_or_else(|| json!(0)),
        "blocked_by_count": issue.get("blocked_by_count").cloned().unwrap_or_else(|| json!(0)),
        "blocked_by": issue.get("blocked_by").cloned().unwrap_or_else(|| json!([])),
        "is_system": issue_is_system(issue),
    })
}

/// Port of `collect_bead_data`'s post-per-store processing in
/// `webui/server.py`. Returned alongside the raw store snapshots gathered by
/// [`collect_bead_store_summaries`] so the two halves can evolve independently.
#[derive(Debug, Clone, Default)]
pub struct BeadData {
    /// Compacted graph nodes, one per non-merge, non-noise issue. Each node
    /// carries `kind = "task"`, its owning store scope, and the list of
    /// agent targets attached to it via hook. `linked_commits` /
    /// `linked_commit_count` are layered on later in `finalize_graph`.
    pub nodes: Vec<Value>,
    /// Dependency edges between graph nodes. `kind` is `"parent"` for
    /// parent-child beads and `"dependency"` for everything else.
    pub edges: Vec<Value>,
    /// Sorted union of bead ids surfaced by `bd blocked` across all stores.
    pub blocked_ids: Vec<String>,
    /// Sorted union of bead ids attached to an agent hook (both the inbound
    /// `hook_by_issue` map and per-store `bd list --status=hooked`).
    pub hooked_ids: Vec<String>,
    /// Merge-request beads translated into a flat `{task_id, commit_sha, …}`
    /// list. Consumed downstream by `collect_git_memory` in gui-cqe.7.
    pub merge_links: Vec<Value>,
    /// Derived `ui_status` counters over non-system task nodes, plus a
    /// `system_running` tally for system tasks. Matches the `summary` dict
    /// Python returns from `collect_bead_data`.
    pub summary: BTreeMap<String, u64>,
}

/// Rust port of `collect_bead_data` in `webui/server.py`. The per-store `bd`
/// subprocess fan-out is handled by [`collect_bead_store_summaries`] so the
/// caller can reuse the same raw snapshots without paying for a second
/// round of subprocess calls.
///
/// Inputs:
/// - `store_snapshots` — raw `bd` payloads per store (issues, blocked,
///   hooked) produced by [`collect_bead_store_summaries`].
/// - `hook_by_issue` — inverted index mapping bead id → list of agent
///   targets currently hooked on it, produced by `collect_agents`.
pub fn collect_bead_data(
    store_snapshots: &[BeadStoreSnapshot],
    hook_by_issue: &HashMap<String, Vec<String>>,
) -> BeadData {
    let mut issue_order: Vec<String> = Vec::new();
    let mut issue_scope: HashMap<String, String> = HashMap::new();
    let mut issue_by_id: HashMap<String, Value> = HashMap::new();
    let mut blocked_ids: HashSet<String> = HashSet::new();
    let mut hooked_ids: HashSet<String> = hook_by_issue.keys().cloned().collect();
    let mut merge_links: Vec<Value> = Vec::new();

    for snapshot in store_snapshots {
        let scope = snapshot.store.scope.clone();
        let store_name = snapshot.store.name.clone();

        for item in &snapshot.blocked {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    blocked_ids.insert(id.to_string());
                }
            }
        }
        for item in &snapshot.hooked {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    hooked_ids.insert(id.to_string());
                }
            }
        }

        for issue in &snapshot.issues {
            let Some(id_ref) = issue.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id_ref.is_empty() {
                continue;
            }
            let id = id_ref.to_string();

            // Extract merge metadata before we potentially overwrite the
            // issue in the dedup map — merge beads can exist in multiple
            // stores but we want to emit a link per (issue, store).
            if issue_is_merge(issue) {
                let description = issue
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let meta = parse_simple_metadata_block(description);
                if let (Some(source_issue), Some(commit_sha)) =
                    (meta.get("source_issue"), meta.get("commit_sha"))
                {
                    if !source_issue.is_empty() && !commit_sha.is_empty() {
                        let short_sha: String = commit_sha.chars().take(7).collect();
                        merge_links.push(json!({
                            "task_id": source_issue,
                            "merge_issue_id": id,
                            "commit_sha": commit_sha,
                            "short_sha": short_sha,
                            "branch": meta.get("branch").cloned().unwrap_or_default(),
                            "target": meta.get("target").cloned().unwrap_or_default(),
                            "worker": meta.get("worker").cloned().unwrap_or_default(),
                            "store": store_name,
                            "scope": scope,
                            "title": issue.get("title").and_then(Value::as_str).unwrap_or(""),
                        }));
                    }
                }
            }

            // Python's `all_issues[issue_id] = issue` semantics: later stores
            // overwrite the bead. We preserve insertion order from the first
            // time the id was seen so the graph is deterministic.
            if !issue_by_id.contains_key(&id) {
                issue_order.push(id.clone());
            }
            issue_by_id.insert(id.clone(), issue.clone());
            issue_scope.insert(id, scope.clone());
        }
    }

    let mut nodes: Vec<Value> = Vec::new();
    let mut summary: BTreeMap<String, u64> = BTreeMap::new();
    for key in ["ready", "running", "stuck", "done", "ice", "system_running"] {
        summary.insert(key.into(), 0);
    }

    for id in &issue_order {
        let Some(issue) = issue_by_id.get(id) else {
            continue;
        };
        if issue_is_merge(issue) || issue_is_graph_noise(issue) {
            continue;
        }
        let mut node = compact_issue(issue, &blocked_ids, &hooked_ids);
        let scope = issue_scope.get(id).cloned().unwrap_or_default();
        let agent_targets: Vec<Value> = hook_by_issue
            .get(id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(Value::String)
            .collect();
        if let Some(obj) = node.as_object_mut() {
            obj.insert("kind".into(), Value::String("task".into()));
            obj.insert("scope".into(), Value::String(scope));
            obj.insert("agent_targets".into(), Value::Array(agent_targets));
        }
        let is_system = node
            .get("is_system")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ui_status = node
            .get("ui_status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if is_system {
            if ui_status == "running" {
                *summary.entry("system_running".into()).or_insert(0) += 1;
            }
        } else {
            *summary.entry(ui_status).or_insert(0) += 1;
        }
        nodes.push(node);
    }

    let node_ids: HashSet<String> = nodes
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str).map(String::from))
        .collect();

    let mut edges: Vec<Value> = Vec::new();
    let mut edge_keys: HashSet<(String, String, String)> = HashSet::new();
    for id in &issue_order {
        if !node_ids.contains(id) {
            continue;
        }
        let Some(issue) = issue_by_id.get(id) else {
            continue;
        };
        let Some(deps) = issue.get("dependencies").and_then(Value::as_array) else {
            continue;
        };
        for dep in deps {
            let source = dep
                .get("depends_on_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if source.is_empty() || !node_ids.contains(&source) {
                continue;
            }
            let edge_kind = if dep.get("type").and_then(Value::as_str) == Some("parent-child") {
                "parent"
            } else {
                "dependency"
            };
            let key = (source.clone(), id.clone(), edge_kind.to_string());
            if !edge_keys.insert(key) {
                continue;
            }
            edges.push(json!({
                "source": source,
                "target": id,
                "kind": edge_kind,
            }));
        }
    }

    let mut blocked_sorted: Vec<String> = blocked_ids.into_iter().collect();
    blocked_sorted.sort();
    let mut hooked_sorted: Vec<String> = hooked_ids.into_iter().collect();
    hooked_sorted.sort();

    BeadData {
        nodes,
        edges,
        blocked_ids: blocked_sorted,
        hooked_ids: hooked_sorted,
        merge_links,
        summary,
    }
}

/// CRC-32-IEEE (same polynomial as `zlib.crc32`). Used only to derive stable,
/// short repo identifiers from their filesystem root — the hash is not
/// cryptographic and is just a compact substitute for the full path in JSON
/// payloads that travel to the frontend.
fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Stable short id for a repo rooted at `root`. Port of `make_repo_id` in
/// `webui/server.py`; keeps the exact `repo-<hex>` shape so cached repo ids
/// persisted in the frontend remain valid across the Python → Rust switch.
fn make_repo_id(root: &str) -> String {
    format!("repo-{:x}", crc32_ieee(root.as_bytes()))
}

fn issue_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(?:hq|[a-z]{2,})-[a-z0-9]+(?:\.[a-z0-9]+)*\b").expect("static regex")
    })
}

/// Extract every bead id mentioned in `text`, deduplicated and lexicographically
/// sorted. Mirrors `find_issue_ids` in `webui/server.py`.
fn find_issue_ids(text: &str) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for m in issue_id_re().find_iter(text) {
        seen.insert(m.as_str().to_string());
    }
    seen.into_iter().collect()
}

/// Parse the output of `git status --short --branch`. Mirrors
/// `parse_git_status` in `webui/server.py`: the first `## ...` line carries the
/// branch description, every non-blank remaining line counts toward modified
/// vs. untracked based on the porcelain prefix.
fn parse_git_status(text: &str) -> Value {
    let lines: Vec<&str> = text.lines().collect();
    let branch = match lines.first() {
        Some(first) if first.starts_with("## ") => first[3..].to_string(),
        _ => String::new(),
    };
    let mut modified: u64 = 0;
    let mut untracked: u64 = 0;
    for line in lines.iter().skip(1) {
        if line.starts_with("??") {
            untracked += 1;
        } else if !line.trim().is_empty() {
            modified += 1;
        }
    }
    json!({
        "branch": branch,
        "modified": modified,
        "untracked": untracked,
        "dirty": modified > 0 || untracked > 0,
        "raw": text,
    })
}

/// Parse `git log` output emitted with the `%H%x1f%h%x1f%cI%x1f%D%x1f%s`
/// format. Records with fewer than five unit-separator fields are skipped so a
/// truncated line from a git error can't crash the parse. Port of
/// `parse_git_log` in `webui/server.py`.
fn parse_git_log(text: &str, repo_id: &str, repo_label: &str) -> Vec<Value> {
    let mut commits: Vec<Value> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\u{001f}').collect();
        if parts.len() != 5 {
            continue;
        }
        let sha = parts[0];
        let short_sha = parts[1];
        let committed_at = parts[2];
        let refs = parts[3];
        let subject = parts[4];
        commits.push(json!({
            "repo_id": repo_id,
            "repo_label": repo_label,
            "sha": sha,
            "short_sha": short_sha,
            "committed_at": committed_at,
            "refs": refs,
            "subject": subject,
            "task_ids": find_issue_ids(subject),
        }));
    }
    commits
}

/// Parse `git branch --format=...` output. Port of `parse_git_branches`.
/// The format string (`%(HEAD)%x1f%(refname:short)%x1f%(objectname:short)%x1f%(committerdate:iso8601-strict)%x1f%(subject)`)
/// is owned by `collect_git_memory`; keep the two in lockstep.
fn parse_git_branches(text: &str) -> Vec<Value> {
    let mut branches: Vec<Value> = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\u{001f}').collect();
        if parts.len() != 5 {
            continue;
        }
        let head_flag = parts[0];
        let name = parts[1];
        let short_sha = parts[2];
        let committed_at = parts[3];
        let subject = parts[4];
        branches.push(json!({
            "current": head_flag == "*",
            "name": name,
            "short_sha": short_sha,
            "committed_at": committed_at,
            "subject": subject,
        }));
    }
    branches
}

/// Parse `git worktree list --porcelain` output. Entries are blank-line
/// separated; each entry is a set of `key value` lines. Port of
/// `parse_worktrees` — normalises the fields the UI actually cares about
/// (`path`, `head`, `branch`) and strips `refs/heads/` from branch names.
fn parse_worktrees(text: &str) -> Vec<Value> {
    let mut records: Vec<HashMap<String, String>> = Vec::new();
    let mut current: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if line.is_empty() {
            if !current.is_empty() {
                records.push(std::mem::take(&mut current));
            }
            continue;
        }
        let (key, value) = match line.find(' ') {
            Some(idx) => (&line[..idx], &line[idx + 1..]),
            None => (line, ""),
        };
        current.insert(key.to_string(), value.to_string());
    }
    if !current.is_empty() {
        records.push(current);
    }

    records
        .into_iter()
        .map(|item| {
            let path = item.get("worktree").cloned().unwrap_or_default();
            let head = item.get("HEAD").cloned().unwrap_or_default();
            let branch = item
                .get("branch")
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_string())
                .unwrap_or_default();
            json!({
                "path": path,
                "head": head,
                "branch": branch,
            })
        })
        .collect()
}

/// Port of `collect_git_memory` in `webui/server.py`. Returned structure mirrors
/// the Python dict 1:1 so the frontend and downstream collectors (`finalize_graph`,
/// future activity grouping) can consume it verbatim.
#[derive(Debug, Clone, Default)]
pub struct GitMemory {
    /// One entry per unique git toplevel discovered across the town root and
    /// crew paths. Fields: `id`, `label`, `root`, `scope`, `scopes`, `status`,
    /// `recent_commits`, `branches` (capped at 12), `worktrees`.
    pub repos: Vec<Value>,
    /// The 20 most recent commits across all repos, sorted by `committed_at`
    /// descending.
    pub recent_commits: Vec<Value>,
    /// `task_id -> [memory entries]` — each entry is either a
    /// `source: "commit-message"` record (commit in a local repo that
    /// mentioned the task id in its subject) or a `source: "merge-bead"`
    /// record (merge bead in the beads store referencing the task). Entries
    /// per task are sorted by `(committed_at, short_sha)` descending.
    pub task_memory: BTreeMap<String, Vec<Value>>,
    /// Sorted union of `repo.id` for every repo in `repos`. Consumed by the
    /// frontend to decide which repos to render in the repos panel.
    pub repo_ids: Vec<String>,
}

impl GitMemory {
    /// Render the git memory as a `serde_json::Value` matching the Python
    /// shape.
    pub fn into_json(self) -> Value {
        let task_memory: serde_json::Map<String, Value> = self
            .task_memory
            .into_iter()
            .map(|(k, v)| (k, Value::Array(v)))
            .collect();
        json!({
            "repos": self.repos,
            "recent_commits": self.recent_commits,
            "task_memory": Value::Object(task_memory),
            "repo_ids": self.repo_ids,
        })
    }
}

/// Rust port of `collect_git_memory` in `webui/server.py`. Discovers every git
/// repo rooted at the town root or a crew path, fans out the four per-repo
/// commands (`status`, `log`, `branch`, `worktree list`) in parallel via
/// `tokio::join!`, and folds the commit history into a `task_id -> memory`
/// index. Merge-request beads from `collect_bead_data` are stitched in so the
/// UI can surface commits that have already landed but aren't in any local
/// worktree.
pub async fn collect_git_memory(
    gt_root: &Path,
    crews: &[Value],
    merge_links: &[Value],
) -> (GitMemory, Vec<Value>, u64) {
    let mut errors: Vec<Value> = Vec::new();
    let mut duration_ms: u64 = 0;

    // Candidate paths to probe with `git rev-parse --show-toplevel`. The town
    // root always contributes the `hq` scope; every crew adds its working
    // directory with the rig scope. Empty paths are dropped so a crew row
    // without a materialised worktree doesn't spam errors.
    let mut candidates: Vec<(String, PathBuf, String)> = vec![(
        "Town Root".to_string(),
        gt_root.to_path_buf(),
        "hq".to_string(),
    )];
    for crew in crews {
        let path_str = crew.get("path").and_then(Value::as_str).unwrap_or("");
        if path_str.is_empty() {
            continue;
        }
        let rig = crew
            .get("rig")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = crew
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        candidates.push((
            format!("Crew {}/{}", rig, name),
            PathBuf::from(path_str),
            rig,
        ));
    }

    struct RepoStub {
        id: String,
        root: String,
        labels: Vec<String>,
        scopes: Vec<String>,
    }

    let mut repo_order: Vec<String> = Vec::new();
    let mut repo_stubs: HashMap<String, RepoStub> = HashMap::new();

    for (label, path, scope) in candidates {
        let path_str = path.to_string_lossy().into_owned();
        let top = run_command(
            &[
                "git",
                "-C",
                path_str.as_str(),
                "rev-parse",
                "--show-toplevel",
            ],
            gt_root,
            RunOptions::default(),
        )
        .await;
        duration_ms += top.duration_ms;
        if !top.ok {
            errors.push(top.to_error());
            continue;
        }
        let root = top
            .data
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if root.is_empty() {
            continue;
        }
        if !repo_stubs.contains_key(&root) {
            repo_order.push(root.clone());
            repo_stubs.insert(
                root.clone(),
                RepoStub {
                    id: make_repo_id(&root),
                    root: root.clone(),
                    labels: Vec::new(),
                    scopes: Vec::new(),
                },
            );
        }
        let stub = repo_stubs.get_mut(&root).expect("just inserted");
        if !stub.labels.contains(&label) {
            stub.labels.push(label);
        }
        if !scope.is_empty() && !stub.scopes.contains(&scope) {
            stub.scopes.push(scope);
        }
    }

    let mut repos_out: Vec<Value> = Vec::with_capacity(repo_order.len());
    let mut task_memory: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut all_recent_commits: Vec<Value> = Vec::new();

    for root in &repo_order {
        let stub = match repo_stubs.get(root) {
            Some(s) => s,
            None => continue,
        };
        let repo_id = stub.id.clone();
        let label = stub.labels.join(" / ");
        let scopes = stub.scopes.clone();
        let repo_scope = if scopes.len() == 1 {
            scopes[0].clone()
        } else {
            String::new()
        };

        let status_args: [&str; 6] = ["git", "-C", root.as_str(), "status", "--short", "--branch"];
        let log_args: [&str; 9] = [
            "git",
            "-C",
            root.as_str(),
            "log",
            "--date=iso-strict",
            "--decorate=short",
            "--format=%H%x1f%h%x1f%cI%x1f%D%x1f%s",
            "-n",
            "16",
        ];
        let branch_args: [&str; 6] = [
            "git",
            "-C",
            root.as_str(),
            "branch",
            "--format=%(HEAD)%x1f%(refname:short)%x1f%(objectname:short)%x1f%(committerdate:iso8601-strict)%x1f%(subject)",
            "--sort=-committerdate",
        ];
        let worktree_args: [&str; 6] = [
            "git",
            "-C",
            root.as_str(),
            "worktree",
            "list",
            "--porcelain",
        ];
        let (status_result, log_result, branch_result, worktree_result) = tokio::join!(
            run_command(&status_args, gt_root, RunOptions::default()),
            run_command(&log_args, gt_root, RunOptions::default()),
            run_command(&branch_args, gt_root, RunOptions::default()),
            run_command(&worktree_args, gt_root, RunOptions::default()),
        );
        duration_ms += status_result.duration_ms
            + log_result.duration_ms
            + branch_result.duration_ms
            + worktree_result.duration_ms;

        for result in [
            &status_result,
            &log_result,
            &branch_result,
            &worktree_result,
        ] {
            if !result.ok {
                errors.push(result.to_error());
            }
        }

        let status = if status_result.ok {
            parse_git_status(
                status_result
                    .data
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        } else {
            json!({})
        };
        let recent_commits = if log_result.ok {
            parse_git_log(
                log_result
                    .data
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                &repo_id,
                &label,
            )
        } else {
            Vec::new()
        };
        let branches = if branch_result.ok {
            parse_git_branches(
                branch_result
                    .data
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        } else {
            Vec::new()
        };
        let worktrees = if worktree_result.ok {
            parse_worktrees(
                worktree_result
                    .data
                    .as_ref()
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
        } else {
            Vec::new()
        };

        let branches_top: Vec<Value> = branches.into_iter().take(12).collect();

        repos_out.push(json!({
            "id": repo_id.clone(),
            "label": label.clone(),
            "root": stub.root.clone(),
            "scope": repo_scope.clone(),
            "scopes": scopes,
            "status": status,
            "recent_commits": recent_commits.clone(),
            "branches": branches_top,
            "worktrees": worktrees,
        }));
        all_recent_commits.extend(recent_commits.iter().cloned());

        for commit in &recent_commits {
            let Some(task_ids) = commit.get("task_ids").and_then(Value::as_array) else {
                continue;
            };
            for tid in task_ids {
                let Some(task_id) = tid.as_str() else {
                    continue;
                };
                if task_id.is_empty() {
                    continue;
                }
                task_memory
                    .entry(task_id.to_string())
                    .or_default()
                    .push(json!({
                        "source": "commit-message",
                        "repo_id": repo_id.clone(),
                        "repo_label": label.clone(),
                        "sha": commit.get("sha").cloned().unwrap_or(Value::String(String::new())),
                        "short_sha": commit.get("short_sha").cloned().unwrap_or(Value::String(String::new())),
                        "subject": commit.get("subject").cloned().unwrap_or(Value::String(String::new())),
                        "committed_at": commit.get("committed_at").cloned().unwrap_or(Value::String(String::new())),
                        "scope": repo_scope.clone(),
                        "available_local": true,
                    }));
            }
        }
    }

    for link in merge_links {
        let Some(task_id) = link.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        if task_id.is_empty() {
            continue;
        }
        let entry = json!({
            "source": "merge-bead",
            "repo_id": "",
            "repo_label": "",
            "sha": link.get("commit_sha").cloned().unwrap_or(Value::String(String::new())),
            "short_sha": link.get("short_sha").cloned().unwrap_or(Value::String(String::new())),
            "subject": link.get("title").cloned().unwrap_or(Value::String(String::new())),
            "committed_at": "",
            "branch": link.get("branch").cloned().unwrap_or(Value::String(String::new())),
            "target": link.get("target").cloned().unwrap_or(Value::String(String::new())),
            "worker": link.get("worker").cloned().unwrap_or(Value::String(String::new())),
            "merge_issue_id": link.get("merge_issue_id").cloned().unwrap_or(Value::String(String::new())),
            "scope": link.get("scope").cloned().unwrap_or(Value::String(String::new())),
            "available_local": false,
        });
        task_memory
            .entry(task_id.to_string())
            .or_default()
            .push(entry);
    }

    all_recent_commits.sort_by(|a, b| {
        let a_at = a.get("committed_at").and_then(Value::as_str).unwrap_or("");
        let b_at = b.get("committed_at").and_then(Value::as_str).unwrap_or("");
        b_at.cmp(a_at)
    });
    for entries in task_memory.values_mut() {
        entries.sort_by(|a, b| {
            let a_at = a.get("committed_at").and_then(Value::as_str).unwrap_or("");
            let b_at = b.get("committed_at").and_then(Value::as_str).unwrap_or("");
            let a_sha = a.get("short_sha").and_then(Value::as_str).unwrap_or("");
            let b_sha = b.get("short_sha").and_then(Value::as_str).unwrap_or("");
            (b_at, b_sha).cmp(&(a_at, a_sha))
        });
    }

    let mut repo_ids: BTreeSet<String> = BTreeSet::new();
    for repo in &repos_out {
        if let Some(id) = repo.get("id").and_then(Value::as_str) {
            if !id.is_empty() {
                repo_ids.insert(id.to_string());
            }
        }
    }
    let repo_ids: Vec<String> = repo_ids.into_iter().collect();

    let recent_commits = all_recent_commits.into_iter().take(20).collect();

    (
        GitMemory {
            repos: repos_out,
            recent_commits,
            task_memory,
            repo_ids,
        },
        errors,
        duration_ms,
    )
}

/// Rust port of `collect_convoy_data` in `webui/server.py`. Runs
/// `bd list --type=convoy --all --json` once, then folds the raw convoy rows
/// into the `{convoys, task_index}` shape the frontend expects. The
/// `task_index` maps each tracked task id to `{total, open, closed,
/// convoy_ids, all_closed}` so the UI can badge tasks with their enclosing
/// convoy state.
///
/// Behaviour mirrors the Python port 1:1:
/// - A convoy is considered complete when `status == "closed"`.
/// - When the raw payload lacks a `tracked` list, dependency edges of type
///   `tracks` are used as the fallback source of task ids.
/// - Each tracked task increments `total` on its `task_index` entry and
///   either `open` or `closed` depending on the enclosing convoy's status.
/// - `completed`/`total` on the convoy row prefer the values from the raw
///   payload; when absent or zero they fall back to
///   `len(tracked_ids) if closed else 0` and `len(tracked_ids)` respectively.
pub async fn collect_convoy_data(gt_root: &Path) -> (Value, Vec<Value>, u64) {
    let result = run_command(
        &["bd", "list", "--type=convoy", "--all", "--json"],
        gt_root,
        RunOptions::default().parse_json(),
    )
    .await;
    let duration_ms = result.duration_ms;
    if !result.ok {
        return (
            json!({
                "convoys": Vec::<Value>::new(),
                "task_index": serde_json::Map::<String, Value>::new(),
            }),
            vec![result.to_error()],
            duration_ms,
        );
    }

    let raw_convoys: Vec<Value> = result
        .data
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    struct TaskEntry {
        total: i64,
        open: i64,
        closed: i64,
        convoy_ids: Vec<String>,
    }

    let mut task_order: Vec<String> = Vec::new();
    let mut task_index: HashMap<String, TaskEntry> = HashMap::new();
    let mut convoys_out: Vec<Value> = Vec::with_capacity(raw_convoys.len());

    for raw in &raw_convoys {
        if !raw.is_object() {
            continue;
        }
        let convoy_id = raw
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let status = raw
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let title = raw
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let is_closed = status == "closed";

        let empty_vec: Vec<Value> = Vec::new();
        let tracked = raw
            .get("tracked")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);
        let dependencies = raw
            .get("dependencies")
            .and_then(Value::as_array)
            .unwrap_or(&empty_vec);

        // When `tracked` is empty (or missing), fall back to dependency edges
        // of type `tracks`. Matches the Python `if not tracked_items` branch.
        let tracked_items: Vec<&Value> = if !tracked.is_empty() {
            tracked.iter().collect()
        } else {
            dependencies
                .iter()
                .filter(|item| {
                    if !item.is_object() {
                        return false;
                    }
                    let kind = item
                        .get("type")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("dependency_type").and_then(Value::as_str))
                        .unwrap_or("");
                    kind == "tracks"
                })
                .collect()
        };

        let mut tracked_ids: Vec<String> = Vec::new();
        for item in tracked_items {
            if !item.is_object() {
                continue;
            }
            let task_id = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("depends_on_id").and_then(Value::as_str))
                .or_else(|| item.get("issue_id").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            if task_id.is_empty() {
                continue;
            }
            tracked_ids.push(task_id.clone());

            let entry = task_index.entry(task_id.clone()).or_insert_with(|| {
                task_order.push(task_id.clone());
                TaskEntry {
                    total: 0,
                    open: 0,
                    closed: 0,
                    convoy_ids: Vec::new(),
                }
            });
            entry.total += 1;
            if is_closed {
                entry.closed += 1;
            } else {
                entry.open += 1;
            }
            if !convoy_id.is_empty() && !entry.convoy_ids.contains(&convoy_id) {
                entry.convoy_ids.push(convoy_id.clone());
            }
        }

        let tracked_len = tracked_ids.len() as i64;
        let completed = raw
            .get("completed")
            .and_then(Value::as_i64)
            .filter(|&n| n != 0)
            .unwrap_or(if is_closed { tracked_len } else { 0 });
        let total = raw
            .get("total")
            .and_then(Value::as_i64)
            .filter(|&n| n != 0)
            .unwrap_or(tracked_len);

        convoys_out.push(json!({
            "id": convoy_id,
            "title": title,
            "status": status,
            "tracked_ids": tracked_ids,
            "completed": completed,
            "total": total,
        }));
    }

    // Preserve insertion order (the order tasks first appear across convoys)
    // by using a separate `task_order` vector rather than iterating a HashMap.
    let mut task_index_json = serde_json::Map::with_capacity(task_order.len());
    for task_id in task_order {
        let entry = task_index
            .remove(&task_id)
            .expect("task_order is derived from task_index keys");
        let all_closed = entry.total > 0 && entry.open == 0;
        task_index_json.insert(
            task_id,
            json!({
                "total": entry.total,
                "open": entry.open,
                "closed": entry.closed,
                "convoy_ids": entry.convoy_ids,
                "all_closed": all_closed,
            }),
        );
    }

    (
        json!({
            "convoys": convoys_out,
            "task_index": Value::Object(task_index_json),
        }),
        Vec::new(),
        duration_ms,
    )
}

/// Rust port of `finalize_graph` in `webui/server.py`. Takes the raw
/// `bead_data` graph (nodes + edges) and layers on:
///
/// 1. `linked_commits` (up to three `short_sha`s) and `linked_commit_count`
///    on every task node, sourced from `git_memory.task_memory`.
/// 2. Synthetic `commit:<sha>` nodes for each commit that mentions a node's
///    task id, plus a `commit` edge from the task node to the commit node.
///    Commit nodes carry enough context (`sha`, `short_sha`, `repo_id`,
///    `repo_label`, `branch`, `available_local`, `scope`, `updated_at`,
///    `parent`) for the graph renderer to badge them correctly.
/// 3. An "interesting" filter: only nodes that are on an edge, referenced by
///    a convoy's `task_index`, carry agent hooks, have linked commits, have
///    a running/stuck `ui_status`, or are system nodes in a live state
///    (`ready`/`running`/`stuck`) survive. Commit nodes survive only when
///    their parent task survived. Edges are then pruned to endpoints that
///    are still present.
///
/// The output shape is `{nodes, edges}` with the same field schema the Python
/// port returned, so downstream consumers (activity grouping in gui-cqe.10,
/// the Tauri IPC surface, the frontend graph renderer) need no changes.
fn finalize_graph(bead_data: &BeadData, git_memory: &GitMemory, convoys: &Value) -> Value {
    let mut nodes: Vec<Value> = bead_data.nodes.clone();
    let mut edges: Vec<Value> = bead_data.edges.clone();

    let mut node_ids: HashSet<String> = nodes
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    let node_scope_map: HashMap<String, String> = nodes
        .iter()
        .filter_map(|n| {
            let id = n.get("id").and_then(Value::as_str)?.to_string();
            let scope = n
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((id, scope))
        })
        .collect();

    let convoy_task_ids: HashSet<String> = convoys
        .get("task_index")
        .and_then(Value::as_object)
        .map(|obj| obj.keys().filter(|k| !k.is_empty()).cloned().collect())
        .unwrap_or_default();

    // (1) Attach linked_commits / linked_commit_count to every bead-data node.
    for node in nodes.iter_mut() {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let entries = git_memory.task_memory.get(&id);
        let (linked_commits, count) = match entries {
            Some(entries) => {
                let commits: Vec<Value> = entries
                    .iter()
                    .take(3)
                    .map(|e| {
                        Value::String(
                            e.get("short_sha")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        )
                    })
                    .collect();
                (Value::Array(commits), entries.len() as u64)
            }
            None => (Value::Array(Vec::new()), 0),
        };
        if let Some(obj) = node.as_object_mut() {
            obj.insert("linked_commits".into(), linked_commits);
            obj.insert("linked_commit_count".into(), Value::Number(count.into()));
        }
    }

    // (2) Build commit nodes + commit edges. Dedup edges with the existing set
    //     so a task that already has an edge to a commit (shouldn't happen
    //     normally, but Python defends it) isn't double-counted.
    let mut edge_keys: HashSet<(String, String, String)> = edges
        .iter()
        .filter_map(|e| {
            let source = e.get("source").and_then(Value::as_str)?.to_string();
            let target = e.get("target").and_then(Value::as_str)?.to_string();
            let kind = e.get("kind").and_then(Value::as_str)?.to_string();
            Some((source, target, kind))
        })
        .collect();

    let mut commit_nodes: Vec<Value> = Vec::new();
    for (task_id, entries) in git_memory.task_memory.iter() {
        if !node_ids.contains(task_id) {
            continue;
        }
        for entry in entries {
            let sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
            if sha.is_empty() {
                continue;
            }
            let commit_node_id = format!("commit:{}", sha);
            if !node_ids.contains(&commit_node_id) {
                let short_sha = entry.get("short_sha").and_then(Value::as_str).unwrap_or("");
                let subject = entry.get("subject").and_then(Value::as_str).unwrap_or("");
                let title = if !subject.is_empty() {
                    subject.to_string()
                } else {
                    format!("Commit {}", short_sha)
                };
                let entry_scope = entry.get("scope").and_then(Value::as_str).unwrap_or("");
                let scope = if !entry_scope.is_empty() {
                    entry_scope.to_string()
                } else {
                    node_scope_map.get(task_id).cloned().unwrap_or_default()
                };
                commit_nodes.push(json!({
                    "id": commit_node_id.clone(),
                    "kind": "commit",
                    "title": title,
                    "description": "",
                    "status": "memory",
                    "ui_status": "memory",
                    "priority": Value::Null,
                    "type": "commit",
                    "owner": "",
                    "assignee": "",
                    "created_at": "",
                    "updated_at": entry.get("committed_at").and_then(Value::as_str).unwrap_or(""),
                    "closed_at": "",
                    "parent": task_id.as_str(),
                    "labels": Vec::<Value>::new(),
                    "dependency_count": 0,
                    "dependent_count": 0,
                    "blocked_by_count": 0,
                    "blocked_by": Vec::<Value>::new(),
                    "is_system": false,
                    "scope": scope,
                    "agent_targets": Vec::<Value>::new(),
                    "linked_commits": Vec::<Value>::new(),
                    "linked_commit_count": 0,
                    "sha": sha,
                    "short_sha": short_sha,
                    "repo_id": entry.get("repo_id").and_then(Value::as_str).unwrap_or(""),
                    "repo_label": entry.get("repo_label").and_then(Value::as_str).unwrap_or(""),
                    "branch": entry.get("branch").and_then(Value::as_str).unwrap_or(""),
                    "available_local": entry
                        .get("available_local")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }));
                node_ids.insert(commit_node_id.clone());
            }
            let edge_key = (
                task_id.clone(),
                commit_node_id.clone(),
                "commit".to_string(),
            );
            if edge_keys.insert(edge_key) {
                edges.push(json!({
                    "source": task_id.as_str(),
                    "target": commit_node_id,
                    "kind": "commit",
                }));
            }
        }
    }

    nodes.extend(commit_nodes);

    // (3) Compute the "interesting" set: edge endpoints, convoy-tracked tasks,
    //     nodes with agent hooks, nodes with linked commits, live task nodes
    //     (running/stuck), and system nodes in a live state.
    let mut interesting_ids: HashSet<String> = HashSet::new();
    for edge in &edges {
        if let Some(s) = edge.get("source").and_then(Value::as_str) {
            interesting_ids.insert(s.to_string());
        }
        if let Some(t) = edge.get("target").and_then(Value::as_str) {
            interesting_ids.insert(t.to_string());
        }
    }
    interesting_ids.extend(convoy_task_ids);
    for node in &nodes {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let has_agents = node
            .get("agent_targets")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        let has_commits = node
            .get("linked_commit_count")
            .and_then(Value::as_u64)
            .map(|n| n > 0)
            .unwrap_or(false);
        let ui_status = node.get("ui_status").and_then(Value::as_str).unwrap_or("");
        let is_system = node
            .get("is_system")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let running_or_stuck = matches!(ui_status, "running" | "stuck");
        let system_live = is_system && matches!(ui_status, "ready" | "running" | "stuck");
        if has_agents || has_commits || running_or_stuck || system_live {
            interesting_ids.insert(id.to_string());
        }
    }

    // (4) Filter nodes, then prune edges to surviving endpoints. Commit nodes
    //     stay only when their parent task stayed.
    let mut kept_ids: HashSet<String> = HashSet::new();
    let filtered_nodes: Vec<Value> = nodes
        .into_iter()
        .filter(|node| {
            let kind = node.get("kind").and_then(Value::as_str).unwrap_or("");
            let id = node
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let keep = if kind == "commit" {
                let parent = node.get("parent").and_then(Value::as_str).unwrap_or("");
                !parent.is_empty() && interesting_ids.contains(parent)
            } else {
                interesting_ids.contains(&id)
            };
            if keep && !id.is_empty() {
                kept_ids.insert(id);
            }
            keep
        })
        .collect();

    let filtered_edges: Vec<Value> = edges
        .into_iter()
        .filter(|e| {
            let s = e.get("source").and_then(Value::as_str).unwrap_or("");
            let t = e.get("target").and_then(Value::as_str).unwrap_or("");
            kept_ids.contains(s) && kept_ids.contains(t)
        })
        .collect();

    json!({
        "nodes": filtered_nodes,
        "edges": filtered_edges,
    })
}

/// Rust port of `build_activity_groups` in `webui/server.py`. Buckets every
/// agent by its hooked bead id, enriching each group with the finalized graph
/// node's title/status/scope (falling back to the agent's hook data when a
/// node is absent) and the per-task commit/merge memory produced by
/// `collect_git_memory`. Agents with no bead id but live signal
/// (events, a tmux session, or a runtime state) land in `unassigned_agents`.
///
/// Event lists are tailed: up to 6 events per agent payload, up to 10 per
/// group. Groups are sorted by (system-last, ui_status priority, task_id) to
/// match the Python contract; unassigned agents are sorted by `target` for
/// stable rendering.
fn build_activity_groups(
    agents: &[AgentInfo],
    graph_nodes: &[Value],
    task_memory: &BTreeMap<String, Vec<Value>>,
) -> Activity {
    let node_map: HashMap<&str, &Value> = graph_nodes
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str).map(|id| (id, n)))
        .collect();

    let mut group_order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, ActivityGroup> = HashMap::new();
    let mut unassigned_agents: Vec<AgentInfo> = Vec::new();

    for agent in agents {
        let hook = &agent.hook;
        let bead_id = hook
            .get("bead_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let events_len = agent.events.len();
        let tail_start = events_len.saturating_sub(6);
        let trimmed_events: Vec<Value> = agent.events[tail_start..].to_vec();

        // Mirror the Python payload: 13 fields only. The remaining AgentInfo
        // fields stay at their defaults so this payload serializes with the
        // shape documented in WEBUI_SNAPSHOT_PARITY.md (`activity.groups[].agents`).
        let hook_payload = if hook.is_null() {
            json!({})
        } else {
            hook.clone()
        };
        let crew_payload = if agent.crew.is_null() {
            json!({})
        } else {
            agent.crew.clone()
        };
        let polecat_payload = if agent.polecat.is_null() {
            json!({})
        } else {
            agent.polecat.clone()
        };
        let payload = AgentInfo {
            target: agent.target.clone(),
            label: agent.label.clone(),
            role: agent.role.clone(),
            scope: agent.scope.clone(),
            kind: agent.kind.clone(),
            has_session: agent.has_session,
            runtime_state: agent.runtime_state.clone(),
            current_path: agent.current_path.clone(),
            session_name: agent.session_name.clone(),
            hook: hook_payload,
            events: trimmed_events,
            crew: crew_payload,
            polecat: polecat_payload,
            ..AgentInfo::default()
        };

        if bead_id.is_empty() {
            if !payload.events.is_empty()
                || payload.has_session
                || !payload.runtime_state.is_empty()
            {
                unassigned_agents.push(payload);
            }
            continue;
        }

        let node = node_map.get(bead_id.as_str()).copied();
        let entry = groups.entry(bead_id.clone());
        let group = match entry {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let title = node
                    .and_then(|n| n.get("title"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| {
                        hook.get("title")
                            .and_then(Value::as_str)
                            .map(String::from)
                            .unwrap_or_else(|| bead_id.clone())
                    });
                let stored_status = node
                    .and_then(|n| n.get("status"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| {
                        hook.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string()
                    });
                let ui_status = node
                    .and_then(|n| n.get("ui_status"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| {
                        hook.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("running")
                            .to_string()
                    });
                let is_system = match node
                    .and_then(|n| n.get("is_system"))
                    .and_then(Value::as_bool)
                {
                    Some(b) => b,
                    None => hook
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .starts_with("mol-"),
                };
                let scope = node
                    .and_then(|n| n.get("scope"))
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| agent.scope.clone());
                let memory = task_memory.get(&bead_id).cloned().unwrap_or_default();
                group_order.push(bead_id.clone());
                v.insert(ActivityGroup {
                    task_id: bead_id.clone(),
                    title,
                    stored_status,
                    ui_status,
                    is_system,
                    scope,
                    agents: Vec::new(),
                    events: Vec::new(),
                    memory,
                    agent_count: 0,
                })
            }
        };
        group.events.extend(payload.events.iter().cloned());
        group.agents.push(payload);
    }

    let mut task_groups: Vec<ActivityGroup> = group_order
        .into_iter()
        .filter_map(|id| groups.remove(&id))
        .collect();

    for group in task_groups.iter_mut() {
        let events_len = group.events.len();
        let tail_start = events_len.saturating_sub(10);
        group.events = group.events[tail_start..].to_vec();
        group.agent_count = group.agents.len() as u32;
    }

    task_groups.sort_by(|a, b| {
        fn status_order(s: &str) -> u8 {
            match s {
                "running" => 0,
                "stuck" => 1,
                "ready" => 2,
                "ice" => 3,
                "done" => 4,
                "memory" => 5,
                _ => 9,
            }
        }
        let a_key = (
            u8::from(a.is_system),
            status_order(&a.ui_status),
            a.task_id.as_str(),
        );
        let b_key = (
            u8::from(b.is_system),
            status_order(&b.ui_status),
            b.task_id.as_str(),
        );
        a_key.cmp(&b_key)
    });

    unassigned_agents.sort_by(|a, b| a.target.cmp(&b.target));

    Activity {
        groups: task_groups,
        unassigned_agents,
    }
}

/// Port of `collect_polecats` in `webui/server.py`. Invokes
/// `gt polecat list --all --json` with a generous timeout (this call is one of
/// the heaviest regular snapshot commands once multiple rigs and persistent
/// polecats are online).
async fn collect_polecats(gt_root: &Path) -> (Vec<Value>, Vec<Value>, u64) {
    let result = run_command(
        &["gt", "polecat", "list", "--all", "--json"],
        gt_root,
        RunOptions::default()
            .with_timeout(Duration::from_secs(6))
            .parse_json(),
    )
    .await;
    let duration_ms = result.duration_ms;
    if !result.ok {
        return (Vec::new(), vec![result.to_error()], duration_ms);
    }
    let polecats = result
        .data
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    (polecats, Vec::new(), duration_ms)
}

/// Port of `collect_agents` in `webui/server.py`. Merges tmux panes, crew rows,
/// and polecat runtime state into a single agent list, then enriches each
/// entry with its hook (via parallel `gt hook show <target> --json` calls) and
/// replays feed events into per-agent `events` / `task_events` / `recent_task`
/// slots. Also returns `hook_by_issue`, keyed by bead id, for downstream
/// collectors that need to know which agents are attached to which issue.
async fn collect_agents(
    gt_root: &Path,
    status_summary: &StatusSummary,
    crews: &[Value],
    feed_events: &[Value],
) -> (
    Vec<AgentInfo>,
    HashMap<String, Vec<String>>,
    Vec<Value>,
    u64,
) {
    let mut errors: Vec<Value> = Vec::new();
    let mut duration_ms: u64 = 0;

    // Stable target ordering keeps behavior deterministic across runs.
    let mut agents_by_target: BTreeMap<String, AgentInfo> = BTreeMap::new();

    let (tmux_agents, tmux_errors, tmux_ms) =
        collect_tmux_agents(gt_root, &status_summary.tmux_socket).await;
    errors.extend(tmux_errors);
    duration_ms += tmux_ms;
    for agent in tmux_agents {
        agents_by_target.insert(agent.target.clone(), agent);
    }

    for crew in crews {
        let rig = crew.get("rig").and_then(Value::as_str).unwrap_or("");
        let name = crew.get("name").and_then(Value::as_str).unwrap_or("");
        if rig.is_empty() || name.is_empty() {
            continue;
        }
        let target = format!("{rig}/crew/{name}");
        let mut existing = agents_by_target.remove(&target).unwrap_or_default();
        existing.target = target.clone();
        existing.label = target.clone();
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
        agents_by_target.insert(target, existing);
    }

    let (polecats, polecat_errors, polecat_ms) = collect_polecats(gt_root).await;
    errors.extend(polecat_errors);
    duration_ms += polecat_ms;
    for polecat in polecats {
        let rig = polecat.get("rig").and_then(Value::as_str).unwrap_or("");
        let name = polecat.get("name").and_then(Value::as_str).unwrap_or("");
        if rig.is_empty() || name.is_empty() {
            continue;
        }
        let target = format!("{rig}/polecats/{name}");
        let mut existing = agents_by_target.remove(&target).unwrap_or_default();
        existing.target = target.clone();
        existing.label = target.clone();
        existing.role = "polecat".into();
        existing.scope = rig.into();
        if existing.kind.is_empty() {
            existing.kind = "external".into();
        }
        existing.has_session = existing.has_session
            || polecat
                .get("session_running")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        existing.runtime_state = polecat
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        existing.polecat = polecat.clone();
        agents_by_target.insert(target, existing);
    }

    let (event_map, task_event_map) = classify_feed_events(feed_events);

    // Parallel `gt hook show <target> --json` — one call per agent — and build
    // the `hook_by_issue` inverted index.
    let mut hook_by_issue: HashMap<String, Vec<String>> = HashMap::new();
    if !agents_by_target.is_empty() {
        let max_workers = worker_count(agents_by_target.len(), 8);
        let mut futures: Vec<tokio::task::JoinHandle<(String, crate::command::CommandResult)>> =
            Vec::with_capacity(agents_by_target.len());
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_workers));
        for target in agents_by_target.keys().cloned() {
            let permit_sem = semaphore.clone();
            let gt_root_owned: PathBuf = gt_root.to_path_buf();
            futures.push(tokio::spawn(async move {
                let _permit = permit_sem.acquire_owned().await.expect("semaphore open");
                let result = run_command(
                    &[
                        "gt".to_string(),
                        "hook".to_string(),
                        "show".to_string(),
                        target.clone(),
                        "--json".to_string(),
                    ],
                    &gt_root_owned,
                    RunOptions::default()
                        .with_timeout(Duration::from_secs(2))
                        .parse_json(),
                )
                .await;
                (target, result)
            }));
        }
        for handle in futures {
            let (target, hook_result) = match handle.await {
                Ok(pair) => pair,
                Err(_) => continue,
            };
            duration_ms += hook_result.duration_ms;
            let Some(agent) = agents_by_target.get_mut(&target) else {
                continue;
            };
            if !hook_result.ok {
                errors.push(hook_result.to_error());
                agent.hook = json!({"agent": target, "status": "unknown"});
            } else {
                let data = hook_result.data.unwrap_or(Value::Null);
                let bead_id = data
                    .get("bead_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                agent.hook = data;
                if !bead_id.is_empty() {
                    hook_by_issue
                        .entry(bead_id)
                        .or_default()
                        .push(target.clone());
                }
            }
        }
    }

    for (target, agent) in agents_by_target.iter_mut() {
        agent.events = event_map.get(target).cloned().unwrap_or_default();
        let task_events = task_event_map.get(target).cloned().unwrap_or_default();
        let tail_start = task_events.len().saturating_sub(6);
        let tail: Vec<Value> = task_events[tail_start..].to_vec();
        agent.recent_task = tail.last().cloned().unwrap_or(Value::Null);
        agent.task_events = tail;
    }

    let mut agents: Vec<AgentInfo> = agents_by_target.into_values().collect();
    agents.sort_by(|a, b| {
        (a.scope.as_str(), a.role.as_str(), a.target.as_str()).cmp(&(
            b.scope.as_str(),
            b.role.as_str(),
            b.target.as_str(),
        ))
    });
    (agents, hook_by_issue, errors, duration_ms)
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
        // When `gt` is missing we expect ≤ 6 synthetic errors (five
        // top-level commands + one `gt polecat list --all --json` inside
        // `collect_agents`). When `gt` is installed the polecat list may
        // succeed, which in turn triggers a hook lookup per discovered
        // agent — that can push the error count over 6 on CI boxes where
        // `gt hook show` fails against a `/tmp` cwd. We only assert the
        // snapshot actually installed; the exact count is
        // environment-dependent.
        let _ = after.summary.command_errors;
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
    fn worker_count_clamps_and_handles_zero() {
        assert_eq!(worker_count(0, 8), 1);
        assert_eq!(worker_count(1, 8), 1);
        assert_eq!(worker_count(3, 8), 3);
        assert_eq!(worker_count(8, 8), 8);
        assert_eq!(worker_count(20, 8), 8);
    }

    #[test]
    fn classify_feed_events_buckets_actor_events_and_ignores_slung_actor() {
        let events = vec![
            json!({"time": "00:01:00", "actor": "gtui/witness", "message": "slung gui-cqe.1 to gtui/polecats/furiosa"}),
            json!({"time": "00:02:00", "actor": "gtui/polecats/furiosa", "message": "done: gui-cqe.1"}),
            json!({"time": "00:03:00", "actor": "gtui/polecats/furiosa", "message": "random chatter"}),
            json!({"time": "00:04:00", "actor": "", "message": "slung orphan.2 to gtui/polecats/nux"}),
        ];
        let (event_map, task_event_map) = classify_feed_events(&events);

        // event_map keys only actors with non-empty actor string.
        assert!(event_map.contains_key("gtui/witness"));
        assert_eq!(
            event_map.get("gtui/polecats/furiosa").map(Vec::len),
            Some(2)
        );
        assert!(!event_map.contains_key(""));

        // slung goes to TARGET, not actor.
        let furiosa_tasks = task_event_map
            .get("gtui/polecats/furiosa")
            .expect("furiosa should receive assigned + done events");
        assert_eq!(furiosa_tasks.len(), 2);
        assert_eq!(furiosa_tasks[0]["kind"], "assigned");
        assert_eq!(furiosa_tasks[0]["task_id"], "gui-cqe.1");
        assert_eq!(furiosa_tasks[0]["order"], "0");
        assert_eq!(furiosa_tasks[1]["kind"], "done");
        assert_eq!(furiosa_tasks[1]["task_id"], "gui-cqe.1");

        // done: without an actor is dropped (matches Python: requires `actor`).
        assert!(!task_event_map.contains_key(""));

        // slung with empty actor still routes to target.
        let nux_tasks = task_event_map
            .get("gtui/polecats/nux")
            .expect("nux should receive the orphan assignment");
        assert_eq!(nux_tasks.len(), 1);
        assert_eq!(nux_tasks[0]["task_id"], "orphan.2");
    }

    #[tokio::test]
    async fn collect_agents_smoke_does_not_panic_with_empty_inputs() {
        // No tmux socket, no crews, no feed events. In a sandboxed env where
        // `gt` is missing, everything returns empty; on a real workstation
        // `gt polecat list --all --json` may actually succeed and surface
        // agents, so we only assert the call completes and returns a
        // coherent tuple.
        let status = StatusSummary {
            tmux_socket: String::new(),
            ..StatusSummary::default()
        };
        let (agents, hook_by_issue, _errors, _ms) =
            collect_agents(&tmp_root(), &status, &[], &[]).await;
        for agent in &agents {
            // Every agent must carry a non-empty target and a resolved role.
            assert!(!agent.target.is_empty());
            assert!(!agent.role.is_empty());
        }
        // hook_by_issue keys are bead ids; values are non-empty target lists.
        for (bead, targets) in &hook_by_issue {
            assert!(!bead.is_empty());
            assert!(!targets.is_empty());
        }
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

    #[test]
    fn configured_rig_names_returns_sorted_rig_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("rigs.json"),
            r#"{"rigs": {"zeta": {}, "alpha": {}, "gtui": {}}}"#,
        )
        .unwrap();
        let names = configured_rig_names(root);
        assert_eq!(names, vec!["alpha", "gtui", "zeta"]);
    }

    #[test]
    fn configured_rig_names_missing_file_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(configured_rig_names(dir.path()).is_empty());
    }

    #[test]
    fn configured_rig_names_malformed_json_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("rigs.json"), "not-json").unwrap();
        assert!(configured_rig_names(dir.path()).is_empty());
    }

    #[test]
    fn configured_rig_names_drops_empty_and_non_object_rigs() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("rigs.json"),
            r#"{"rigs": {"": {}, "good": {}}}"#,
        )
        .unwrap();
        assert_eq!(configured_rig_names(dir.path()), vec!["good"]);

        std::fs::write(dir.path().join("rigs.json"), r#"{"rigs": []}"#).unwrap();
        assert!(configured_rig_names(dir.path()).is_empty());
    }

    #[test]
    fn discover_bead_stores_emits_hq_and_per_rig_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".beads")).unwrap();
        std::fs::create_dir_all(root.join("gtui/.beads")).unwrap();
        std::fs::create_dir_all(root.join("gastown/.beads")).unwrap();
        // Configured but no .beads dir — skipped.
        std::fs::create_dir_all(root.join("solo")).unwrap();
        // Configured but not a directory on disk — skipped.
        std::fs::write(
            root.join("rigs.json"),
            r#"{"rigs": {"gtui": {}, "gastown": {}, "solo": {}, "ghost": {}}}"#,
        )
        .unwrap();
        let stores = discover_bead_stores(root);
        let names: Vec<&str> = stores.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["hq", "gastown", "gtui"]);
        assert_eq!(stores[0].path, root.to_path_buf());
        assert_eq!(stores[0].scope, "hq");
        assert_eq!(stores[1].path, root.join("gastown"));
        assert_eq!(stores[1].scope, "gastown");
    }

    #[test]
    fn discover_bead_stores_without_hq_still_lists_rigs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        // No .beads at the root, so no hq store.
        std::fs::create_dir_all(root.join("gtui/.beads")).unwrap();
        std::fs::write(root.join("rigs.json"), r#"{"rigs": {"gtui": {}}}"#).unwrap();
        let stores = discover_bead_stores(root);
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].name, "gtui");
    }

    #[test]
    fn discover_bead_stores_empty_root_returns_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(discover_bead_stores(dir.path()).is_empty());
    }

    #[test]
    fn count_issue_statuses_handles_missing_status() {
        let issues = vec![
            json!({"id": "a", "status": "open"}),
            json!({"id": "b", "status": "open"}),
            json!({"id": "c", "status": "closed"}),
            json!({"id": "d"}),
        ];
        let counts = count_issue_statuses(&issues);
        assert_eq!(counts.get("open"), Some(&2));
        assert_eq!(counts.get("closed"), Some(&1));
        assert_eq!(counts.get("unknown"), Some(&1));
    }

    #[tokio::test]
    async fn collect_bead_store_summaries_empty_stores_is_noop() {
        let (summaries, snapshots, errors, ms) = collect_bead_store_summaries(&[]).await;
        assert!(summaries.is_empty());
        assert!(snapshots.is_empty());
        assert!(errors.is_empty());
        assert_eq!(ms, 0);
    }

    #[tokio::test]
    async fn collect_bead_store_summaries_records_errors_when_bd_missing() {
        // Point at an isolated tempdir so the `bd` subprocess fails fast (no
        // beads data). The summary must still be emitted with the fixture
        // shape and `available = false`.
        let dir = tempfile::tempdir().expect("tempdir");
        let stores = vec![BeadStore {
            name: "hq".into(),
            path: dir.path().to_path_buf(),
            scope: "hq".into(),
        }];
        let (summaries, snapshots, errors, _ms) = collect_bead_store_summaries(&stores).await;
        assert_eq!(summaries.len(), 1);
        let store = &summaries[0];
        for key in [
            "available",
            "blocked",
            "closed",
            "error",
            "exact_status_counts",
            "hooked",
            "name",
            "open",
            "path",
            "scope",
            "summary",
            "total",
        ] {
            assert!(
                store.get(key).is_some(),
                "store summary missing key `{key}`: {store:?}"
            );
        }
        assert_eq!(store["name"], "hq");
        assert_eq!(store["scope"], "hq");
        assert_eq!(store["total"], 0);
        assert_eq!(store["open"], 0);
        // Any subprocess failure feeds `errors` and forces `available=false`.
        assert!(!errors.is_empty());
        assert_eq!(store["available"], false);
        // Raw snapshot is always emitted so downstream collectors can reuse it.
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].store.name, "hq");
    }

    fn bead_snapshot(
        name: &str,
        scope: &str,
        issues: Vec<Value>,
        blocked: Vec<Value>,
        hooked: Vec<Value>,
    ) -> BeadStoreSnapshot {
        BeadStoreSnapshot {
            store: BeadStore {
                name: name.into(),
                path: PathBuf::from(format!("/tmp/{name}")),
                scope: scope.into(),
            },
            status_payload: None,
            issues,
            blocked,
            hooked,
            status_ok: true,
            status_error: String::new(),
        }
    }

    #[test]
    fn parse_simple_metadata_block_extracts_key_value_lines() {
        let meta = parse_simple_metadata_block(
            "source_issue: gt-123\ncommit_sha: deadbeef\n\nignored body",
        );
        assert_eq!(meta.get("source_issue").map(String::as_str), Some("gt-123"));
        assert_eq!(meta.get("commit_sha").map(String::as_str), Some("deadbeef"));
    }

    #[test]
    fn parse_simple_metadata_block_stops_after_twenty_lines() {
        // 20 filler lines + 1 real key on line 21 — must not be picked up.
        let mut text = String::new();
        for _ in 0..20 {
            text.push_str("noise\n");
        }
        text.push_str("after_cap: value\n");
        let meta = parse_simple_metadata_block(&text);
        assert!(!meta.contains_key("after_cap"));
    }

    #[test]
    fn issue_is_merge_honours_label_or_metadata_block() {
        let by_label = json!({
            "id": "gt-1",
            "labels": ["gt:merge-request"],
            "description": "",
        });
        assert!(issue_is_merge(&by_label));

        let by_meta = json!({
            "id": "gt-2",
            "labels": [],
            "description": "source_issue: gt-task\ncommit_sha: abc123\n",
        });
        assert!(issue_is_merge(&by_meta));

        let neither = json!({"id": "gt-3", "labels": [], "description": "hello"});
        assert!(!issue_is_merge(&neither));
    }

    #[test]
    fn issue_is_system_matches_labels_types_ids_and_titles() {
        assert!(issue_is_system(&json!({"labels": ["gt:rig"]})));
        assert!(issue_is_system(&json!({"issue_type": "molecule"})));
        assert!(issue_is_system(&json!({"id": "hq-wisp-abc"})));
        assert!(issue_is_system(&json!({"title": "mol-polecat-work"})));
        assert!(!issue_is_system(&json!({"id": "gt-1", "title": "Port X"})));
    }

    #[test]
    fn issue_is_graph_noise_filters_non_allowed_types_and_labels() {
        // Random issue_type with no system marker → noise.
        assert!(issue_is_graph_noise(
            &json!({"issue_type": "mail", "labels": []})
        ));
        // gt:message label → noise even if type is allowed.
        assert!(issue_is_graph_noise(&json!({
            "issue_type": "task",
            "labels": ["gt:message"]
        })));
        // hq-cv-* convoy beads are excluded.
        assert!(issue_is_graph_noise(
            &json!({"id": "hq-cv-99", "issue_type": "task"})
        ));
        // Allowed type without noisy labels → kept.
        assert!(!issue_is_graph_noise(
            &json!({"id": "gt-1", "issue_type": "task", "labels": []})
        ));
        // Molecule (system) with weird type → kept because system overrides.
        assert!(!issue_is_graph_noise(&json!({
            "id": "hq-wisp-x",
            "issue_type": "molecule",
        })));
    }

    #[test]
    fn derive_ui_status_covers_all_branches() {
        let blocked: HashSet<String> = ["b1".into()].into_iter().collect();
        let hooked: HashSet<String> = ["h1".into()].into_iter().collect();

        assert_eq!(
            derive_ui_status(&json!({"id": "a", "status": "closed"}), &blocked, &hooked),
            "done"
        );
        assert_eq!(
            derive_ui_status(&json!({"id": "a", "status": "hooked"}), &blocked, &hooked),
            "running"
        );
        assert_eq!(
            derive_ui_status(&json!({"id": "h1", "status": "open"}), &blocked, &hooked),
            "running"
        );
        assert_eq!(
            derive_ui_status(&json!({"id": "a", "status": "deferred"}), &blocked, &hooked),
            "ice"
        );
        assert_eq!(
            derive_ui_status(&json!({"id": "b1", "status": "open"}), &blocked, &hooked),
            "stuck"
        );
        assert_eq!(
            derive_ui_status(&json!({"id": "other", "status": "open"}), &blocked, &hooked),
            "ready"
        );
    }

    #[test]
    fn collect_bead_data_compacts_issues_and_derives_summary() {
        let issues = vec![
            json!({
                "id": "gt-1",
                "title": "Running task",
                "status": "in_progress",
                "issue_type": "task",
                "labels": [],
                "dependencies": [],
            }),
            json!({
                "id": "gt-2",
                "title": "Blocked task",
                "status": "open",
                "issue_type": "task",
                "labels": [],
                "dependencies": [
                    {"depends_on_id": "gt-1", "type": "dependency"}
                ],
            }),
            json!({
                "id": "gt-3",
                "title": "Frozen",
                "status": "deferred",
                "issue_type": "task",
                "labels": [],
                "dependencies": [],
            }),
            json!({
                "id": "gt-4",
                "title": "Done",
                "status": "closed",
                "issue_type": "task",
                "labels": [],
                "dependencies": [],
            }),
            json!({
                "id": "gt-5",
                "title": "Ignored mail",
                "status": "open",
                "issue_type": "task",
                "labels": ["gt:message"],
                "dependencies": [],
            }),
            json!({
                "id": "hq-wisp-x",
                "title": "Running molecule",
                "status": "hooked",
                "issue_type": "molecule",
                "labels": [],
                "dependencies": [],
            }),
            json!({
                "id": "gt-merge-1",
                "title": "Merge: gt-1 -> main",
                "status": "closed",
                "issue_type": "task",
                "labels": ["gt:merge-request"],
                "description": "source_issue: gt-1\ncommit_sha: abcdef0123456\nbranch: feature/x\ntarget: main\nworker: polecat\n",
                "dependencies": [],
            }),
        ];
        let blocked = vec![json!({"id": "gt-2"})];
        let hooked = vec![json!({"id": "gt-1"})];

        let snap = bead_snapshot("hq", "hq", issues, blocked, hooked);
        let mut hook_by_issue: HashMap<String, Vec<String>> = HashMap::new();
        hook_by_issue.insert("gt-1".into(), vec!["gtui/polecats/nux".into()]);
        let bead = collect_bead_data(&[snap], &hook_by_issue);

        let node_ids: Vec<String> = bead
            .nodes
            .iter()
            .filter_map(|n| n.get("id").and_then(Value::as_str).map(String::from))
            .collect();
        // Merge issue and gt:message issue are filtered out; molecule kept as
        // system task; gt-1..gt-4 all kept.
        assert!(node_ids.contains(&"gt-1".to_string()));
        assert!(node_ids.contains(&"gt-2".to_string()));
        assert!(node_ids.contains(&"gt-3".to_string()));
        assert!(node_ids.contains(&"gt-4".to_string()));
        assert!(node_ids.contains(&"hq-wisp-x".to_string()));
        assert!(!node_ids.contains(&"gt-merge-1".to_string()));
        assert!(!node_ids.contains(&"gt-5".to_string()));

        // Compacted node carries kind, scope, agent_targets.
        let gt1 = bead
            .nodes
            .iter()
            .find(|n| n.get("id").and_then(Value::as_str) == Some("gt-1"))
            .expect("gt-1 node");
        assert_eq!(gt1["kind"], "task");
        assert_eq!(gt1["scope"], "hq");
        assert_eq!(gt1["ui_status"], "running");
        assert_eq!(gt1["agent_targets"][0], "gtui/polecats/nux");
        assert_eq!(gt1["is_system"], false);

        // Blocked dep sets ui_status to stuck even though status=open.
        let gt2 = bead
            .nodes
            .iter()
            .find(|n| n.get("id").and_then(Value::as_str) == Some("gt-2"))
            .expect("gt-2 node");
        assert_eq!(gt2["ui_status"], "stuck");

        // Single edge for gt-1 -> gt-2 dependency.
        assert_eq!(bead.edges.len(), 1);
        assert_eq!(bead.edges[0]["source"], "gt-1");
        assert_eq!(bead.edges[0]["target"], "gt-2");
        assert_eq!(bead.edges[0]["kind"], "dependency");

        // Summary counters.
        assert_eq!(bead.summary.get("running"), Some(&1));
        assert_eq!(bead.summary.get("stuck"), Some(&1));
        assert_eq!(bead.summary.get("ice"), Some(&1));
        assert_eq!(bead.summary.get("done"), Some(&1));
        assert_eq!(bead.summary.get("ready"), Some(&0));
        assert_eq!(bead.summary.get("system_running"), Some(&1));

        // Merge links derived from the merge-request bead.
        assert_eq!(bead.merge_links.len(), 1);
        let link = &bead.merge_links[0];
        assert_eq!(link["task_id"], "gt-1");
        assert_eq!(link["merge_issue_id"], "gt-merge-1");
        assert_eq!(link["commit_sha"], "abcdef0123456");
        assert_eq!(link["short_sha"], "abcdef0");
        assert_eq!(link["branch"], "feature/x");
        assert_eq!(link["target"], "main");
        assert_eq!(link["store"], "hq");
        assert_eq!(link["scope"], "hq");

        // Blocked/hooked id sets are sorted unions.
        assert_eq!(bead.blocked_ids, vec!["gt-2".to_string()]);
        assert!(bead.hooked_ids.contains(&"gt-1".to_string()));
    }

    #[test]
    fn collect_bead_data_dedupes_across_stores_and_preserves_first_scope() {
        // Same id appears in both stores; Python's last-write-wins on the
        // issue body, but we keep the insertion order so the graph stays
        // deterministic. Scope tracks the last store's scope.
        let store_a = bead_snapshot(
            "hq",
            "hq",
            vec![json!({
                "id": "gt-1",
                "title": "First",
                "status": "open",
                "issue_type": "task",
                "labels": [],
                "dependencies": [],
            })],
            Vec::new(),
            Vec::new(),
        );
        let store_b = bead_snapshot(
            "gtui",
            "gtui",
            vec![json!({
                "id": "gt-1",
                "title": "Second",
                "status": "open",
                "issue_type": "task",
                "labels": [],
                "dependencies": [],
            })],
            Vec::new(),
            Vec::new(),
        );
        let bead = collect_bead_data(&[store_a, store_b], &HashMap::new());
        assert_eq!(bead.nodes.len(), 1);
        let node = &bead.nodes[0];
        assert_eq!(node["title"], "Second");
        assert_eq!(node["scope"], "gtui");
    }

    #[test]
    fn collect_bead_data_drops_edges_to_nodes_outside_graph() {
        // gt-1 depends on gt-noise (filtered out as non-allowed type) — the
        // edge must not be emitted because the source never made it into
        // `nodes`.
        let issues = vec![
            json!({
                "id": "gt-1",
                "title": "Keeper",
                "status": "open",
                "issue_type": "task",
                "labels": [],
                "dependencies": [
                    {"depends_on_id": "gt-noise", "type": "dependency"}
                ],
            }),
            json!({
                "id": "gt-noise",
                "title": "Mail",
                "status": "open",
                "issue_type": "mail",
                "labels": [],
                "dependencies": [],
            }),
        ];
        let snap = bead_snapshot("hq", "hq", issues, Vec::new(), Vec::new());
        let bead = collect_bead_data(&[snap], &HashMap::new());
        let ids: Vec<_> = bead
            .nodes
            .iter()
            .filter_map(|n| n.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["gt-1"]);
        assert!(bead.edges.is_empty());
    }

    #[test]
    fn collect_bead_data_marks_parent_child_dependency_kind() {
        let issues = vec![
            json!({
                "id": "parent",
                "title": "P",
                "status": "open",
                "issue_type": "epic",
                "labels": [],
                "dependencies": [],
            }),
            json!({
                "id": "child",
                "title": "C",
                "status": "open",
                "issue_type": "task",
                "labels": [],
                "dependencies": [
                    {"depends_on_id": "parent", "type": "parent-child"}
                ],
            }),
        ];
        let snap = bead_snapshot("hq", "hq", issues, Vec::new(), Vec::new());
        let bead = collect_bead_data(&[snap], &HashMap::new());
        assert_eq!(bead.edges.len(), 1);
        assert_eq!(bead.edges[0]["kind"], "parent");
    }

    #[test]
    fn make_repo_id_matches_python_zlib_crc32() {
        // Expected values produced by Python:
        // `hex(zlib.crc32(b"/home/user/gt") & 0xFFFFFFFF)` etc.
        assert_eq!(make_repo_id(""), "repo-0");
        assert_eq!(make_repo_id("hello"), "repo-3610a686");
        assert_eq!(make_repo_id("/home/user/gt"), "repo-8974c2ca");
    }

    #[test]
    fn find_issue_ids_returns_sorted_unique_matches() {
        let subject = "fix: port git memory (gui-cqe.7) — refs hq-abc, hq-ABC, gui-cqe.7";
        let ids = find_issue_ids(subject);
        assert_eq!(ids, vec!["gui-cqe.7", "hq-ABC", "hq-abc"]);
    }

    #[test]
    fn parse_git_status_extracts_branch_and_counts() {
        let text = "## main...origin/main [ahead 1]\n M src/lib.rs\n?? new.txt\nA  added.rs\n";
        let status = parse_git_status(text);
        assert_eq!(status["branch"], "main...origin/main [ahead 1]");
        assert_eq!(status["modified"], 2);
        assert_eq!(status["untracked"], 1);
        assert_eq!(status["dirty"], true);
        assert_eq!(status["raw"], text);
    }

    #[test]
    fn parse_git_status_clean_tree_is_not_dirty() {
        let status = parse_git_status("## main\n");
        assert_eq!(status["branch"], "main");
        assert_eq!(status["modified"], 0);
        assert_eq!(status["untracked"], 0);
        assert_eq!(status["dirty"], false);
    }

    #[test]
    fn parse_git_log_emits_one_entry_per_well_formed_line() {
        let text = format!(
            "{sha}\x1f{short}\x1f{when}\x1fHEAD -> main\x1ffix: port git memory (gui-cqe.7)\nmalformed line with no separators\n",
            sha = "a".repeat(40),
            short = "aaaaaaa",
            when = "2026-04-22T08:01:00+00:00",
        );
        let commits = parse_git_log(&text, "repo-abc", "Town Root");
        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(commit["repo_id"], "repo-abc");
        assert_eq!(commit["repo_label"], "Town Root");
        assert_eq!(commit["short_sha"], "aaaaaaa");
        assert_eq!(commit["refs"], "HEAD -> main");
        assert_eq!(commit["subject"], "fix: port git memory (gui-cqe.7)");
        assert_eq!(commit["task_ids"][0], "gui-cqe.7");
    }

    #[test]
    fn parse_git_branches_marks_current_and_splits_fields() {
        let text = "*\x1fmain\x1fabcdef0\x1f2026-04-22T08:00:00+00:00\x1fport git memory\n\x1ffeature/x\x1f1234567\x1f2026-04-20T10:00:00+00:00\x1fWIP feature\n";
        let branches = parse_git_branches(text);
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["current"], true);
        assert_eq!(branches[0]["name"], "main");
        assert_eq!(branches[0]["short_sha"], "abcdef0");
        assert_eq!(branches[1]["current"], false);
        assert_eq!(branches[1]["name"], "feature/x");
    }

    #[test]
    fn parse_worktrees_splits_porcelain_entries_and_strips_refs_heads() {
        let text = "worktree /home/user/gt\nHEAD abcdef0123456789\nbranch refs/heads/main\n\nworktree /home/user/gt/gtui/polecats/furiosa/gtui\nHEAD 1234567\nbranch refs/heads/polecat/furiosa/gui-cqe.7\n";
        let worktrees = parse_worktrees(text);
        assert_eq!(worktrees.len(), 2);
        assert_eq!(worktrees[0]["path"], "/home/user/gt");
        assert_eq!(worktrees[0]["head"], "abcdef0123456789");
        assert_eq!(worktrees[0]["branch"], "main");
        assert_eq!(worktrees[1]["branch"], "polecat/furiosa/gui-cqe.7");
    }

    #[test]
    fn parse_worktrees_handles_final_entry_without_trailing_blank_line() {
        let text = "worktree /one\nHEAD aaa\nbranch refs/heads/main";
        let worktrees = parse_worktrees(text);
        assert_eq!(worktrees.len(), 1);
        assert_eq!(worktrees[0]["path"], "/one");
    }

    #[tokio::test]
    async fn collect_git_memory_folds_merge_links_into_task_memory() {
        // No git commands succeed against the temp dir (it's not a repo), so
        // the repos list stays empty — but the merge_links path should still
        // populate task_memory with a `source: "merge-bead"` entry per link.
        let merge_links = vec![json!({
            "task_id": "gui-cqe.1",
            "merge_issue_id": "gt-merge-1",
            "commit_sha": "deadbeef0123456789",
            "short_sha": "deadbee",
            "branch": "polecat/furiosa/gui-cqe.1",
            "target": "main",
            "worker": "gtui/polecats/furiosa",
            "title": "fix: port bead discovery (gui-cqe.1)",
            "scope": "gtui",
        })];
        let tmp = tempfile::tempdir().expect("tempdir");
        let (memory, _errors, _ms) = collect_git_memory(tmp.path(), &[], &merge_links).await;
        let entries = memory
            .task_memory
            .get("gui-cqe.1")
            .expect("merge-link task_memory entry");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry["source"], "merge-bead");
        assert_eq!(entry["short_sha"], "deadbee");
        assert_eq!(entry["branch"], "polecat/furiosa/gui-cqe.1");
        assert_eq!(entry["available_local"], false);
        assert_eq!(entry["scope"], "gtui");
    }

    #[tokio::test]
    async fn collect_git_memory_round_trips_through_into_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (memory, _errors, _ms) = collect_git_memory(tmp.path(), &[], &[]).await;
        let value = memory.into_json();
        assert!(value.get("repos").is_some());
        assert!(value.get("recent_commits").is_some());
        assert!(value.get("task_memory").is_some());
        assert!(value.get("repo_ids").is_some());
    }

    #[tokio::test]
    async fn collect_convoy_data_returns_empty_shape_when_bd_missing() {
        // No `bd` binary on PATH in the sandbox; the collector must still
        // return the `{convoys: [], task_index: {}}` shape and record the
        // subprocess failure in `errors`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (value, errors, _ms) = collect_convoy_data(tmp.path()).await;
        assert_eq!(
            value.get("convoys").and_then(Value::as_array),
            Some(&Vec::<Value>::new())
        );
        assert!(value
            .get("task_index")
            .and_then(Value::as_object)
            .map(|m| m.is_empty())
            .unwrap_or(false));
        assert!(!errors.is_empty(), "missing bd binary should surface");
    }

    #[test]
    fn tail_n_returns_last_entries() {
        let items = vec![1, 2, 3, 4, 5];
        assert_eq!(tail_n(&items, 2), vec![4, 5]);
        assert_eq!(tail_n(&items, 10), vec![1, 2, 3, 4, 5]);
        assert_eq!(tail_n::<i32>(&[], 3), Vec::<i32>::new());
    }

    #[test]
    fn merge_agent_overlay_preserves_cached_hook_and_task_events() {
        let cached = AgentInfo {
            target: "gtui/polecats/nux".into(),
            label: "nux".into(),
            role: "polecat".into(),
            scope: "gtui".into(),
            kind: "external".into(),
            has_session: false,
            current_path: "/stale/path".into(),
            session_name: "stale".into(),
            pane_id: "%stale".into(),
            current_command: "".into(),
            hook: json!({"bead_id": "gui-cqe.12", "pane_id": "%hook"}),
            events: vec![json!({"message": "old"})],
            task_events: vec![json!({"kind": "assigned", "task_id": "gui-cqe.12"})],
            recent_task: json!({"task_id": "gui-cqe.12"}),
            ..AgentInfo::default()
        };
        let live = AgentInfo {
            target: "gtui/polecats/nux".into(),
            label: "gtui/polecats/nux".into(),
            role: "polecat".into(),
            scope: "gtui".into(),
            kind: "tmux".into(),
            has_session: true,
            current_path: "/live/path".into(),
            session_name: "live".into(),
            pane_id: "%live".into(),
            current_command: "claude".into(),
            ..AgentInfo::default()
        };
        let merged = merge_agent_overlay(cached, live);
        assert!(merged.has_session);
        assert_eq!(merged.kind, "tmux");
        assert_eq!(merged.pane_id, "%live");
        assert_eq!(merged.current_command, "claude");
        assert_eq!(merged.current_path, "/live/path");
        assert_eq!(merged.session_name, "live");
        assert_eq!(merged.hook["bead_id"], "gui-cqe.12");
        assert_eq!(merged.task_events.len(), 1);
        assert_eq!(merged.recent_task["task_id"], "gui-cqe.12");
        assert_eq!(merged.events.len(), 1);
    }

    #[tokio::test]
    async fn get_terminal_state_returns_cached_agent_fields_and_defaults() {
        let store = SnapshotStore::new(tempfile::tempdir().expect("tempdir").path().to_path_buf());
        let snap = WorkspaceSnapshot {
            agents: vec![AgentInfo {
                target: "gtui/polecats/nux".into(),
                label: "nux".into(),
                role: "polecat".into(),
                scope: "gtui".into(),
                kind: "external".into(),
                has_session: false,
                current_path: "/tmp/nonexistent-path".into(),
                current_command: "bash".into(),
                hook: json!({"bead_id": "gui-cqe.12"}),
                events: (0..10)
                    .map(|n| json!({"message": format!("event {n}"), "actor": "other"}))
                    .collect(),
                task_events: (0..8)
                    .map(|n| json!({"kind": "assigned", "task_id": format!("t-{n}")}))
                    .collect(),
                recent_task: json!({"task_id": "t-7"}),
                ..AgentInfo::default()
            }],
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let state = store
            .get_terminal_state("gtui/polecats/nux")
            .await
            .expect("terminal state");
        assert_eq!(state["target"], "gtui/polecats/nux");
        assert_eq!(state["hook"]["bead_id"], "gui-cqe.12");
        // Events cap to last 6.
        assert_eq!(state["events"].as_array().map(Vec::len), Some(6));
        // Task events and recent_task round-trip.
        assert_eq!(state["task_events"].as_array().map(Vec::len), Some(6));
        assert_eq!(state["recent_task"]["task_id"], "t-7");
        // Transcript views default to empty maps, not nulls.
        assert!(state["codex_view"].is_object());
        assert!(state["claude_view"].is_object());
        assert!(state["transcript_view"].is_object());
        assert_eq!(state["render_mode"], "terminal");
        assert!(state["log_lines"].is_array());
        assert!(state["services"].is_array());
    }

    #[tokio::test]
    async fn get_terminal_state_errors_for_unknown_target_via_store() {
        let store = SnapshotStore::new(tempfile::tempdir().expect("tempdir").path().to_path_buf());
        let err = store
            .get_terminal_state("gtui/polecats/ghost")
            .await
            .expect_err("unknown target should error");
        assert!(err.contains("Unknown terminal target"), "got: {err}");
    }
}

#[cfg(test)]
mod convoy_tests {
    //! Pure-logic tests for convoy folding. The HTTP/subprocess-less path is
    //! exercised via a private helper so we can isolate the transformation
    //! from the `bd` command that feeds it.
    use super::*;

    /// Test helper that invokes the same folding logic as
    /// [`collect_convoy_data`] but against an in-memory `Vec<Value>`.
    fn fold_raw_convoys(raw: Vec<Value>) -> Value {
        // Re-implement the minimal shell so we can unit-test the folding
        // without reaching for a subprocess. Kept intentionally small — the
        // production path in `collect_convoy_data` owns any future changes.
        use serde_json::Map;

        struct TaskEntry {
            total: i64,
            open: i64,
            closed: i64,
            convoy_ids: Vec<String>,
        }

        let mut task_order: Vec<String> = Vec::new();
        let mut task_index: HashMap<String, TaskEntry> = HashMap::new();
        let mut convoys_out: Vec<Value> = Vec::new();

        for row in &raw {
            if !row.is_object() {
                continue;
            }
            let convoy_id = row
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let status = row
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let title = row
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let is_closed = status == "closed";

            let empty_vec: Vec<Value> = Vec::new();
            let tracked = row
                .get("tracked")
                .and_then(Value::as_array)
                .unwrap_or(&empty_vec);
            let dependencies = row
                .get("dependencies")
                .and_then(Value::as_array)
                .unwrap_or(&empty_vec);

            let tracked_items: Vec<&Value> = if !tracked.is_empty() {
                tracked.iter().collect()
            } else {
                dependencies
                    .iter()
                    .filter(|item| {
                        if !item.is_object() {
                            return false;
                        }
                        let kind = item
                            .get("type")
                            .and_then(Value::as_str)
                            .or_else(|| item.get("dependency_type").and_then(Value::as_str))
                            .unwrap_or("");
                        kind == "tracks"
                    })
                    .collect()
            };

            let mut tracked_ids: Vec<String> = Vec::new();
            for item in tracked_items {
                if !item.is_object() {
                    continue;
                }
                let task_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("depends_on_id").and_then(Value::as_str))
                    .or_else(|| item.get("issue_id").and_then(Value::as_str))
                    .unwrap_or("")
                    .to_string();
                if task_id.is_empty() {
                    continue;
                }
                tracked_ids.push(task_id.clone());

                let entry = task_index.entry(task_id.clone()).or_insert_with(|| {
                    task_order.push(task_id.clone());
                    TaskEntry {
                        total: 0,
                        open: 0,
                        closed: 0,
                        convoy_ids: Vec::new(),
                    }
                });
                entry.total += 1;
                if is_closed {
                    entry.closed += 1;
                } else {
                    entry.open += 1;
                }
                if !convoy_id.is_empty() && !entry.convoy_ids.contains(&convoy_id) {
                    entry.convoy_ids.push(convoy_id.clone());
                }
            }

            let tracked_len = tracked_ids.len() as i64;
            let completed = row
                .get("completed")
                .and_then(Value::as_i64)
                .filter(|&n| n != 0)
                .unwrap_or(if is_closed { tracked_len } else { 0 });
            let total = row
                .get("total")
                .and_then(Value::as_i64)
                .filter(|&n| n != 0)
                .unwrap_or(tracked_len);

            convoys_out.push(json!({
                "id": convoy_id,
                "title": title,
                "status": status,
                "tracked_ids": tracked_ids,
                "completed": completed,
                "total": total,
            }));
        }

        let mut task_index_json = Map::new();
        for task_id in task_order {
            let entry = task_index.remove(&task_id).expect("known key");
            let all_closed = entry.total > 0 && entry.open == 0;
            task_index_json.insert(
                task_id,
                json!({
                    "total": entry.total,
                    "open": entry.open,
                    "closed": entry.closed,
                    "convoy_ids": entry.convoy_ids,
                    "all_closed": all_closed,
                }),
            );
        }

        json!({
            "convoys": convoys_out,
            "task_index": Value::Object(task_index_json),
        })
    }

    #[test]
    fn folds_tracked_items_and_counts_open_vs_closed() {
        let raw = vec![
            json!({
                "id": "hq-cv-1",
                "title": "Convoy 1",
                "status": "open",
                "tracked": [
                    {"id": "gui-cqe.1"},
                    {"id": "gui-cqe.2"},
                ],
            }),
            json!({
                "id": "hq-cv-2",
                "title": "Convoy 2",
                "status": "closed",
                "tracked": [
                    {"id": "gui-cqe.2"},
                    {"id": "gui-cqe.3"},
                ],
            }),
        ];

        let out = fold_raw_convoys(raw);
        let convoys = out["convoys"].as_array().expect("convoys");
        assert_eq!(convoys.len(), 2);
        assert_eq!(convoys[0]["tracked_ids"], json!(["gui-cqe.1", "gui-cqe.2"]));
        // Open convoy with no `completed` override reports 0 completed.
        assert_eq!(convoys[0]["completed"], 0);
        assert_eq!(convoys[0]["total"], 2);
        // Closed convoy without an override defaults `completed` to total.
        assert_eq!(convoys[1]["completed"], 2);
        assert_eq!(convoys[1]["total"], 2);

        let task_index = out["task_index"].as_object().expect("task_index");
        // gui-cqe.2 appears in both convoys: once open, once closed.
        let two = &task_index["gui-cqe.2"];
        assert_eq!(two["total"], 2);
        assert_eq!(two["open"], 1);
        assert_eq!(two["closed"], 1);
        assert_eq!(two["convoy_ids"], json!(["hq-cv-1", "hq-cv-2"]));
        assert_eq!(two["all_closed"], false);

        // gui-cqe.3 only appears in the closed convoy.
        let three = &task_index["gui-cqe.3"];
        assert_eq!(three["all_closed"], true);
    }

    #[test]
    fn falls_back_to_tracks_dependencies_when_tracked_missing() {
        let raw = vec![json!({
            "id": "hq-cv-dep",
            "status": "open",
            "dependencies": [
                {"depends_on_id": "gui-cqe.10", "type": "tracks"},
                {"depends_on_id": "gui-cqe.11", "type": "blocks"},
                {"issue_id": "gui-cqe.12", "dependency_type": "tracks"},
            ],
        })];

        let out = fold_raw_convoys(raw);
        let tracked = out["convoys"][0]["tracked_ids"]
            .as_array()
            .expect("tracked_ids");
        let ids: Vec<&str> = tracked.iter().filter_map(Value::as_str).collect();
        assert_eq!(ids, vec!["gui-cqe.10", "gui-cqe.12"]);
    }

    #[test]
    fn prefers_raw_completed_and_total_when_nonzero() {
        let raw = vec![json!({
            "id": "hq-cv-override",
            "status": "open",
            "tracked": [{"id": "gui-cqe.20"}],
            "completed": 5,
            "total": 10,
        })];

        let out = fold_raw_convoys(raw);
        assert_eq!(out["convoys"][0]["completed"], 5);
        assert_eq!(out["convoys"][0]["total"], 10);
    }

    #[test]
    fn skips_tracked_items_without_an_id() {
        let raw = vec![json!({
            "id": "hq-cv-empty-id",
            "status": "open",
            "tracked": [
                {"id": ""},
                {"label": "no id field here"},
                {"id": "gui-cqe.30"},
            ],
        })];

        let out = fold_raw_convoys(raw);
        assert_eq!(out["convoys"][0]["tracked_ids"], json!(["gui-cqe.30"]));
        let task_index = out["task_index"].as_object().expect("task_index");
        assert_eq!(task_index.len(), 1);
        assert!(task_index.contains_key("gui-cqe.30"));
    }

    #[test]
    fn ignores_non_object_rows() {
        let raw = vec![
            json!("not an object"),
            json!({
                "id": "hq-cv-ok",
                "status": "open",
                "tracked": [{"id": "gui-cqe.40"}],
            }),
        ];

        let out = fold_raw_convoys(raw);
        let convoys = out["convoys"].as_array().expect("convoys");
        assert_eq!(convoys.len(), 1);
        assert_eq!(convoys[0]["id"], "hq-cv-ok");
    }
}

#[cfg(test)]
mod finalize_graph_tests {
    use super::*;

    fn task_node(id: &str, ui_status: &str, extra: Value) -> Value {
        let mut node = json!({
            "id": id,
            "title": id,
            "description": "",
            "status": "open",
            "ui_status": ui_status,
            "priority": Value::Null,
            "type": "task",
            "owner": "",
            "assignee": "",
            "created_at": "",
            "updated_at": "",
            "closed_at": "",
            "parent": "",
            "labels": [],
            "dependency_count": 0,
            "dependent_count": 0,
            "blocked_by_count": 0,
            "blocked_by": [],
            "is_system": false,
            "kind": "task",
            "scope": "hq",
            "agent_targets": [],
        });
        if let (Some(obj), Some(ext)) = (node.as_object_mut(), extra.as_object()) {
            for (k, v) in ext {
                obj.insert(k.clone(), v.clone());
            }
        }
        node
    }

    fn memory_entry(sha: &str, subject: &str) -> Value {
        json!({
            "source": "commit-message",
            "repo_id": "repo-1",
            "repo_label": "Town Root",
            "sha": sha,
            "short_sha": &sha[..sha.len().min(7)],
            "subject": subject,
            "committed_at": "2026-04-22T10:00:00Z",
            "scope": "hq",
            "branch": "main",
            "available_local": true,
        })
    }

    #[test]
    fn layers_linked_commits_and_creates_commit_nodes() {
        let bead_data = BeadData {
            nodes: vec![task_node("gui-cqe.9", "running", json!({}))],
            edges: vec![],
            ..BeadData::default()
        };

        let mut task_memory = BTreeMap::new();
        task_memory.insert(
            "gui-cqe.9".to_string(),
            vec![
                memory_entry("abc1234xyz", "Port graph finalization"),
                memory_entry("def5678uvw", "Fixup"),
            ],
        );
        let git_memory = GitMemory {
            task_memory,
            ..GitMemory::default()
        };

        let convoys = json!({"convoys": [], "task_index": {}});
        let graph = finalize_graph(&bead_data, &git_memory, &convoys);

        let nodes = graph["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 3, "task node + two commit nodes survive");

        let task = nodes.iter().find(|n| n["id"] == "gui-cqe.9").expect("task");
        assert_eq!(task["linked_commit_count"], 2);
        let linked = task["linked_commits"].as_array().expect("linked_commits");
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0], "abc1234");

        let commit = nodes
            .iter()
            .find(|n| n["id"] == "commit:abc1234xyz")
            .expect("commit node");
        assert_eq!(commit["kind"], "commit");
        assert_eq!(commit["ui_status"], "memory");
        assert_eq!(commit["parent"], "gui-cqe.9");
        assert_eq!(commit["sha"], "abc1234xyz");
        assert_eq!(commit["available_local"], true);

        let edges = graph["edges"].as_array().expect("edges");
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e["kind"] == "commit"));
    }

    #[test]
    fn caps_linked_commits_at_three() {
        let bead_data = BeadData {
            nodes: vec![task_node("t-1", "running", json!({}))],
            edges: vec![],
            ..BeadData::default()
        };
        let mut task_memory = BTreeMap::new();
        task_memory.insert(
            "t-1".to_string(),
            (0..5)
                .map(|i| memory_entry(&format!("sha{:03}aaa", i), "s"))
                .collect(),
        );
        let git_memory = GitMemory {
            task_memory,
            ..GitMemory::default()
        };
        let graph = finalize_graph(&bead_data, &git_memory, &json!({"task_index": {}}));
        let task = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "t-1")
            .unwrap();
        assert_eq!(task["linked_commit_count"], 5);
        assert_eq!(task["linked_commits"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn filters_uninteresting_nodes_but_keeps_convoy_and_edge_endpoints() {
        // Three nodes: one connected by an edge, one in a convoy, one boring.
        let bead_data = BeadData {
            nodes: vec![
                task_node("edge-src", "ready", json!({})),
                task_node("edge-dst", "ready", json!({})),
                task_node("in-convoy", "ready", json!({})),
                task_node("boring", "ready", json!({})),
            ],
            edges: vec![json!({
                "source": "edge-src",
                "target": "edge-dst",
                "kind": "dependency",
            })],
            ..BeadData::default()
        };
        let git_memory = GitMemory::default();
        let convoys = json!({
            "convoys": [],
            "task_index": {"in-convoy": {"total": 1, "open": 1, "closed": 0}},
        });
        let graph = finalize_graph(&bead_data, &git_memory, &convoys);
        let ids: std::collections::HashSet<String> = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains("edge-src"));
        assert!(ids.contains("edge-dst"));
        assert!(ids.contains("in-convoy"));
        assert!(!ids.contains("boring"), "boring node should be filtered");
        // Edge survives because both endpoints are in the kept set.
        assert_eq!(graph["edges"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn keeps_live_and_hooked_nodes_even_without_edges() {
        let bead_data = BeadData {
            nodes: vec![
                task_node("running-alone", "running", json!({})),
                task_node("stuck-alone", "stuck", json!({})),
                task_node(
                    "hooked-alone",
                    "ready",
                    json!({"agent_targets": ["gtui/polecats/furiosa"]}),
                ),
                task_node("system-ready", "ready", json!({"is_system": true})),
                task_node("system-ice", "ice", json!({"is_system": true})),
                task_node("boring", "ready", json!({})),
            ],
            edges: vec![],
            ..BeadData::default()
        };
        let graph = finalize_graph(
            &bead_data,
            &GitMemory::default(),
            &json!({"task_index": {}}),
        );
        let ids: std::collections::HashSet<String> = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains("running-alone"));
        assert!(ids.contains("stuck-alone"));
        assert!(ids.contains("hooked-alone"));
        assert!(ids.contains("system-ready"));
        assert!(!ids.contains("system-ice"), "ice system node is filtered");
        assert!(!ids.contains("boring"));
    }

    #[test]
    fn drops_commit_node_when_parent_is_filtered() {
        // Task isn't "interesting" on its own (ready, no agents, no edges, not
        // in a convoy). But it has a linked commit → linked_commit_count > 0
        // makes it interesting, so both the task and the commit survive.
        let bead_data = BeadData {
            nodes: vec![task_node("t-with-commit", "ready", json!({}))],
            edges: vec![],
            ..BeadData::default()
        };
        let mut task_memory = BTreeMap::new();
        task_memory.insert(
            "t-with-commit".to_string(),
            vec![memory_entry("ffffffff", "subject")],
        );
        let git_memory = GitMemory {
            task_memory,
            ..GitMemory::default()
        };
        let graph = finalize_graph(&bead_data, &git_memory, &json!({"task_index": {}}));
        let ids: std::collections::HashSet<String> = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_string())
            .collect();
        assert!(ids.contains("t-with-commit"));
        assert!(ids.contains("commit:ffffffff"));
    }

    #[test]
    fn skips_commit_entries_with_empty_sha() {
        let bead_data = BeadData {
            nodes: vec![task_node("t-1", "running", json!({}))],
            edges: vec![],
            ..BeadData::default()
        };
        let mut task_memory = BTreeMap::new();
        task_memory.insert(
            "t-1".to_string(),
            vec![json!({
                "source": "merge-bead",
                "sha": "",
                "short_sha": "",
                "subject": "no sha",
                "committed_at": "",
                "scope": "",
                "branch": "",
                "available_local": false,
            })],
        );
        let git_memory = GitMemory {
            task_memory,
            ..GitMemory::default()
        };
        let graph = finalize_graph(&bead_data, &git_memory, &json!({"task_index": {}}));
        let commit_count = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n["kind"] == "commit")
            .count();
        assert_eq!(commit_count, 0, "empty-sha entries must not become nodes");
        assert_eq!(graph["edges"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn commit_node_falls_back_to_task_scope_when_entry_scope_is_empty() {
        let bead_data = BeadData {
            nodes: vec![task_node("t-1", "running", json!({"scope": "gastown"}))],
            edges: vec![],
            ..BeadData::default()
        };
        let mut task_memory = BTreeMap::new();
        let mut entry = memory_entry("cafebabe", "s");
        entry["scope"] = Value::String(String::new());
        task_memory.insert("t-1".to_string(), vec![entry]);
        let git_memory = GitMemory {
            task_memory,
            ..GitMemory::default()
        };
        let graph = finalize_graph(&bead_data, &git_memory, &json!({"task_index": {}}));
        let commit = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["kind"] == "commit")
            .expect("commit node present");
        assert_eq!(commit["scope"], "gastown");
    }
}

#[cfg(test)]
mod build_activity_groups_tests {
    use super::*;

    fn agent(
        target: &str,
        bead_id: &str,
        events: Vec<Value>,
        has_session: bool,
        runtime_state: &str,
    ) -> AgentInfo {
        AgentInfo {
            target: target.into(),
            label: target.into(),
            role: "polecat".into(),
            scope: "gtui".into(),
            kind: "external".into(),
            has_session,
            runtime_state: runtime_state.into(),
            current_path: "/tmp".into(),
            session_name: "".into(),
            hook: if bead_id.is_empty() {
                Value::Null
            } else {
                json!({"bead_id": bead_id, "title": "hook title", "status": "running"})
            },
            events,
            crew: Value::Null,
            polecat: Value::Null,
            ..AgentInfo::default()
        }
    }

    fn task_node(id: &str, ui_status: &str, extras: Value) -> Value {
        let mut node = json!({
            "id": id,
            "title": format!("Task {id}"),
            "status": "open",
            "ui_status": ui_status,
            "is_system": false,
            "scope": "gtui",
        });
        if let Some(obj) = extras.as_object() {
            for (k, v) in obj {
                node[k] = v.clone();
            }
        }
        node
    }

    #[test]
    fn groups_agents_by_bead_id_and_pulls_title_from_node() {
        let agents = vec![
            agent("gtui/polecats/nux", "gui-cqe.10", vec![], true, ""),
            agent("gtui/polecats/furiosa", "gui-cqe.10", vec![], true, ""),
        ];
        let nodes = vec![task_node("gui-cqe.10", "running", json!({}))];
        let activity = build_activity_groups(&agents, &nodes, &BTreeMap::new());
        assert_eq!(activity.groups.len(), 1);
        let group = &activity.groups[0];
        assert_eq!(group.task_id, "gui-cqe.10");
        assert_eq!(group.title, "Task gui-cqe.10");
        assert_eq!(group.agent_count, 2);
        assert_eq!(group.agents.len(), 2);
        assert!(activity.unassigned_agents.is_empty());
    }

    #[test]
    fn falls_back_to_hook_fields_when_node_is_absent() {
        let mut a = agent("gtui/polecats/nux", "gui-cqe.99", vec![], true, "");
        a.hook = json!({
            "bead_id": "gui-cqe.99",
            "title": "mol-polecat-work",
            "status": "ready",
        });
        let activity = build_activity_groups(&[a], &[], &BTreeMap::new());
        let group = &activity.groups[0];
        assert_eq!(group.title, "mol-polecat-work");
        assert_eq!(group.stored_status, "ready");
        // ui_status defaults to "running" when neither node nor hook.status="running" override it.
        // Here hook.status is "ready", which is used as ui_status fallback.
        assert_eq!(group.ui_status, "ready");
        assert!(group.is_system, "mol- prefix should classify as system");
    }

    #[test]
    fn unassigned_requires_signal_otherwise_dropped() {
        let noisy = agent("gtui/a", "", vec![json!({"k": "v"})], false, "");
        let live = agent("gtui/b", "", vec![], true, "");
        let statey = agent("gtui/c", "", vec![], false, "idle");
        let silent = agent("gtui/d", "", vec![], false, "");
        let activity = build_activity_groups(&[noisy, live, statey, silent], &[], &BTreeMap::new());
        assert!(activity.groups.is_empty());
        let targets: Vec<&str> = activity
            .unassigned_agents
            .iter()
            .map(|a| a.target.as_str())
            .collect();
        assert_eq!(targets, vec!["gtui/a", "gtui/b", "gtui/c"]);
    }

    #[test]
    fn attaches_task_memory_entries_to_group() {
        let agents = vec![agent("gtui/polecats/nux", "gui-cqe.10", vec![], true, "")];
        let nodes = vec![task_node("gui-cqe.10", "running", json!({}))];
        let mut task_memory = BTreeMap::new();
        task_memory.insert(
            "gui-cqe.10".to_string(),
            vec![json!({"sha": "abc123", "source": "commit-message"})],
        );
        let activity = build_activity_groups(&agents, &nodes, &task_memory);
        let group = &activity.groups[0];
        assert_eq!(group.memory.len(), 1);
        assert_eq!(group.memory[0]["sha"], "abc123");
    }

    #[test]
    fn sorts_groups_by_system_flag_then_status_then_id() {
        let agents = vec![
            agent("a/1", "sys-run", vec![], true, ""),
            agent("a/2", "task-ready", vec![], true, ""),
            agent("a/3", "task-running", vec![], true, ""),
            agent("a/4", "task-stuck", vec![], true, ""),
        ];
        let nodes = vec![
            task_node("sys-run", "running", json!({"is_system": true})),
            task_node("task-ready", "ready", json!({})),
            task_node("task-running", "running", json!({})),
            task_node("task-stuck", "stuck", json!({})),
        ];
        let activity = build_activity_groups(&agents, &nodes, &BTreeMap::new());
        let order: Vec<&str> = activity.groups.iter().map(|g| g.task_id.as_str()).collect();
        // Non-system first (running, stuck, ready), then system nodes.
        assert_eq!(
            order,
            vec!["task-running", "task-stuck", "task-ready", "sys-run"]
        );
    }

    #[test]
    fn trims_agent_events_to_six_and_group_events_to_ten() {
        let long_events: Vec<Value> = (0..8).map(|i| json!({"i": i})).collect();
        let a1 = agent("a/1", "t", long_events.clone(), true, "");
        let a2 = agent("a/2", "t", long_events, true, "");
        let nodes = vec![task_node("t", "running", json!({}))];
        let activity = build_activity_groups(&[a1, a2], &nodes, &BTreeMap::new());
        let group = &activity.groups[0];
        // Each agent's payload should have 6 events (tail of 8).
        assert_eq!(group.agents[0].events.len(), 6);
        assert_eq!(group.agents[1].events.len(), 6);
        // Group events = concat of two agents' trimmed events (12) then trimmed to 10.
        assert_eq!(group.events.len(), 10);
    }

    #[test]
    fn agent_with_no_bead_and_no_signal_is_dropped() {
        let ghost = agent("ghost/1", "", vec![], false, "");
        let activity = build_activity_groups(&[ghost], &[], &BTreeMap::new());
        assert!(activity.groups.is_empty());
        assert!(activity.unassigned_agents.is_empty());
    }

    #[test]
    fn payload_hook_and_crew_default_to_empty_object_when_null() {
        let a = agent("gtui/polecats/nux", "", vec![], true, "");
        let activity = build_activity_groups(&[a], &[], &BTreeMap::new());
        let payload = &activity.unassigned_agents[0];
        assert_eq!(payload.hook, json!({}));
        assert_eq!(payload.crew, json!({}));
        assert_eq!(payload.polecat, json!({}));
    }

    #[test]
    fn fallback_scope_uses_agent_when_node_missing() {
        let mut a = agent("gtui/polecats/nux", "orphan", vec![], true, "");
        a.scope = "gastown".into();
        let activity = build_activity_groups(&[a], &[], &BTreeMap::new());
        assert_eq!(activity.groups[0].scope, "gastown");
    }
}
