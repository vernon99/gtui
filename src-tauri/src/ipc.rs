//! Tauri IPC command handlers.
//!
//! Each handler takes the shared [`SnapshotStore`] as `State` and returns
//! `Result<T, String>` so the JS frontend sees either a decoded JSON value or
//! a string error via `window.__TAURI__.core.invoke`.

use serde_json::Value;
use tauri::State;

use crate::models::WorkspaceSnapshot;
use crate::snapshot::{CachedGitDiff, SnapshotStore};

/// Return the latest workspace snapshot.
#[tauri::command]
pub async fn get_snapshot(store: State<'_, SnapshotStore>) -> Result<WorkspaceSnapshot, String> {
    Ok(store.get())
}

/// Return terminal and transcript state for one target.
#[tauri::command]
pub async fn get_terminal(
    store: State<'_, SnapshotStore>,
    target: String,
) -> Result<Value, String> {
    store.get_terminal_state(&target).await
}

/// Return a cached or freshly computed git diff.
#[tauri::command]
pub async fn get_git_diff(
    store: State<'_, SnapshotStore>,
    repo: String,
    sha: String,
) -> Result<CachedGitDiff, String> {
    store.fetch_diff(&repo, &sha).await
}

/// Bring up the Gas Town runtime.
#[tauri::command]
pub async fn run_gt(store: State<'_, SnapshotStore>) -> Result<Value, String> {
    store.run_gt().await
}

/// Pause the Gas Town runtime.
#[tauri::command]
pub async fn stop_gt(store: State<'_, SnapshotStore>) -> Result<Value, String> {
    store.stop_gt().await
}

/// Retry a task through the Gas Town CLI.
#[tauri::command]
pub async fn retry_task(store: State<'_, SnapshotStore>, task_id: String) -> Result<Value, String> {
    store.retry_task(&task_id).await
}

/// Ask an agent to pause after its current step.
#[tauri::command]
pub async fn pause_agent(
    store: State<'_, SnapshotStore>,
    agent_id: String,
) -> Result<Value, String> {
    store.pause_agent(&agent_id).await
}

/// Send an instruction to an agent.
#[tauri::command]
pub async fn inject_message(
    store: State<'_, SnapshotStore>,
    agent_id: String,
    message: String,
) -> Result<Value, String> {
    store.inject_instruction(&agent_id, &message).await
}

/// Write text into an agent's tmux pane.
#[tauri::command]
pub async fn write_terminal(
    store: State<'_, SnapshotStore>,
    agent_id: String,
    text: String,
) -> Result<Value, String> {
    store.write_terminal(&agent_id, &text).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AgentInfo, WorkspaceSnapshot};
    use serde_json::json;
    use std::path::PathBuf;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir()
    }

    fn missing_root() -> PathBuf {
        std::env::temp_dir().join("gtui-ipc-missing-root")
    }

    #[tokio::test]
    async fn get_snapshot_returns_current_store_frame() {
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            gt_root: "/tmp/ipc-test".into(),
            alerts: vec!["hello".into()],
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap.clone());
        let fetched = store.get();
        assert_eq!(fetched.alerts, vec!["hello".to_string()]);
        assert_eq!(fetched.gt_root, "/tmp/ipc-test");
    }

    #[tokio::test]
    async fn fetch_diff_errors_on_unknown_repo_id() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .fetch_diff("no-such-repo", "deadbeef")
            .await
            .expect_err("should error");
        assert!(err.contains("Unknown repo id"), "got: {err}");
    }

    #[tokio::test]
    async fn fetch_diff_uses_repo_root_from_snapshot() {
        // When the snapshot exposes a known repo id, fetch_diff resolves the
        // root and invokes `git` against it. We don't have a real git repo
        // here — the point is to prove the lookup works, so we expect the
        // command to fail (Err) rather than "Unknown repo id".
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            git: json!({
                "repos": [
                    {"id": "some-repo", "root": "/nonexistent/path"}
                ]
            }),
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let err = store
            .fetch_diff("some-repo", "deadbeef")
            .await
            .expect_err("should error");
        assert!(
            !err.contains("Unknown repo id"),
            "expected a git failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn run_gt_records_control_action() {
        let store = SnapshotStore::new(missing_root());
        let action = store.run_gt().await.expect("run_gt returns action payload");
        assert_eq!(action["kind"], "run-gt");
        assert!(
            action["command"]
                .as_str()
                .unwrap_or("")
                .contains("gt up --restore --quiet"),
            "got: {}",
            action["command"]
        );
        assert!(action.get("ok").is_some());
    }

    #[tokio::test]
    async fn stop_gt_records_control_action() {
        let store = SnapshotStore::new(missing_root());
        let action = store
            .stop_gt()
            .await
            .expect("stop_gt returns action payload");
        assert_eq!(action["kind"], "stop-gt");
        assert!(
            action["command"]
                .as_str()
                .unwrap_or("")
                .contains("gt down --polecats --quiet"),
            "got: {}",
            action["command"]
        );
        assert!(action.get("ok").is_some());
    }

    #[tokio::test]
    async fn retry_task_errors_on_unknown_task() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .retry_task("gui-missing")
            .await
            .expect_err("should error");
        assert!(err.contains("Unknown task"), "got: {err}");
    }

    #[tokio::test]
    async fn retry_task_errors_on_non_retryable_status() {
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            graph: json!({
                "nodes": [
                    {"id": "gui-open", "status": "open"}
                ],
                "edges": [],
            }),
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let err = store
            .retry_task("gui-open")
            .await
            .expect_err("should error");
        assert!(err.contains("not in a retryable"), "got: {err}");
    }

    #[tokio::test]
    async fn retry_task_errors_when_hooked_without_agent() {
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            graph: json!({
                "nodes": [
                    {"id": "gui-hooked", "status": "hooked", "agent_targets": []}
                ],
                "edges": [],
            }),
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let err = store
            .retry_task("gui-hooked")
            .await
            .expect_err("should error");
        assert!(err.contains("no hooked agent"), "got: {err}");
    }

    #[tokio::test]
    async fn inject_instruction_rejects_blank_message() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .inject_instruction("gtui/polecats/nux", "   ")
            .await
            .expect_err("should error");
        assert!(err.contains("empty"), "got: {err}");
    }

    #[tokio::test]
    async fn write_terminal_rejects_blank_message() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .write_terminal("gtui/polecats/nux", "")
            .await
            .expect_err("should error");
        assert!(err.contains("empty"), "got: {err}");
    }

    #[tokio::test]
    async fn write_terminal_errors_on_unknown_target() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .write_terminal("gtui/polecats/ghost", "hello")
            .await
            .expect_err("should error");
        assert!(err.contains("Unknown terminal target"), "got: {err}");
    }

    #[tokio::test]
    async fn write_terminal_errors_when_agent_has_no_session() {
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            agents: vec![AgentInfo {
                target: "gtui/polecats/dormant".into(),
                has_session: false,
                ..AgentInfo::default()
            }],
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let err = store
            .write_terminal("gtui/polecats/dormant", "hello")
            .await
            .expect_err("should error");
        assert!(err.contains("does not currently have"), "got: {err}");
    }

    #[tokio::test]
    async fn get_terminal_state_errors_for_unknown_target() {
        let store = SnapshotStore::new(tmp_root());
        let err = store
            .get_terminal_state("gtui/polecats/ghost")
            .await
            .expect_err("should error");
        assert!(err.contains("Unknown terminal target"), "got: {err}");
    }

    #[test]
    fn repo_root_lookup_round_trips() {
        let store = SnapshotStore::new(tmp_root());
        assert!(store.get_repo_root("anything").is_none());
        let snap = WorkspaceSnapshot {
            git: json!({
                "repos": [
                    {"id": "gtui", "root": "/home/user/gt/gtui"},
                    {"id": "gastown", "root": "/home/user/gt/gastown"},
                ]
            }),
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        assert_eq!(
            store.get_repo_root("gastown").as_deref(),
            Some("/home/user/gt/gastown")
        );
        assert!(store.get_repo_root("missing").is_none());
    }

    #[test]
    fn get_node_looks_up_graph_node() {
        let store = SnapshotStore::new(tmp_root());
        let snap = WorkspaceSnapshot {
            graph: json!({
                "nodes": [
                    {"id": "gui-1", "status": "open"},
                    {"id": "gui-2", "status": "hooked"},
                ],
                "edges": [],
            }),
            ..WorkspaceSnapshot::default()
        };
        store.install_snapshot(snap);
        let node = store.get_node("gui-2").expect("node present");
        assert_eq!(node["status"], "hooked");
        assert!(store.get_node("gui-3").is_none());
    }

    /// Compile-time check: if any of these function items is deleted or
    /// renamed, the `generate_handler!` macro fails to compile, which in
    /// turn breaks this helper and — transitively — the test suite.
    fn _register_all_commands<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
        builder.invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_terminal,
            get_git_diff,
            run_gt,
            stop_gt,
            retry_task,
            pause_agent,
            inject_message,
            write_terminal,
        ])
    }

    #[test]
    fn all_commands_are_registered_with_the_builder() {
        // Building the helper above is sufficient; the presence of this test
        // keeps it reachable even under `--tests`-only configurations.
        let _ = _register_all_commands::<tauri::Wry>;
    }
}
