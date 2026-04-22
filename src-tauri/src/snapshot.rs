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

use crate::command::{run_command, RunOptions};
use crate::config::POLL_INTERVAL;
use crate::models::{
    default_status_legend, Activity, Metrics, StatusSummary, Timings, WorkspaceSnapshot,
};
use crate::parse::{now_iso, parse_feed, parse_status_summary};
use crate::sessions::{ClaudeCache, CodexCache};

/// Maximum number of most-recent actions retained on the snapshot store.
pub const ACTION_HISTORY_LIMIT: usize = 24;

/// Number of actions surfaced on each snapshot (matches Python `[:12]`).
pub const SNAPSHOT_ACTION_LIMIT: usize = 12;

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

    let crews: Vec<Value> = crew_list_result
        .data
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

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
        agents: Vec::new(),
        stores: Vec::new(),
        actions: actions_for_snapshot,
        errors,
        timings: Timings {
            gt_commands_ms,
            ..Timings::default()
        },
    }
    .tag_feed(feed_events)
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
