//! Integration tests covering the snapshot store's full flow: building a
//! snapshot with the `gt` binary absent, recording actions, dedup-by-
//! fingerprint, and bubbling errors up to the UI-visible `errors` vector.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use gtui_lib::models::WorkspaceSnapshot;
use gtui_lib::snapshot::{
    build_snapshot, fingerprint_snapshot, CachedGitDiff, SnapshotStore, ACTION_HISTORY_LIMIT,
    SNAPSHOT_ACTION_LIMIT,
};
use serde_json::json;

/// A path that definitely isn't a Gas Town root. Used to keep the background
/// poller's CWD deterministic on every developer's box.
fn isolated_root() -> PathBuf {
    std::env::temp_dir().join("gtui-integration-test")
}

#[tokio::test]
async fn build_snapshot_records_errors_when_gt_binary_missing() {
    // Empty action history so the snapshot's `actions` field ends up empty.
    let snapshot = build_snapshot(&isolated_root(), &[]).await;

    // Regardless of whether `gt` is present, the snapshot layer must fill the
    // always-present fields and never panic on absent subcommands.
    assert!(!snapshot.generated_at.is_empty());
    assert_eq!(snapshot.gt_root, isolated_root().to_string_lossy());
    assert!(
        snapshot.summary.command_errors as usize == snapshot.errors.len(),
        "summary.command_errors ({}) diverged from errors.len() ({})",
        snapshot.summary.command_errors,
        snapshot.errors.len()
    );
}

#[tokio::test]
async fn refresh_once_installs_and_suppresses_duplicates() {
    let store = SnapshotStore::new(isolated_root());
    // First refresh must install (fingerprint differs from the zero default).
    assert!(store.refresh_once().await);

    // Second refresh in isolation — the snapshots differ in wall-clock, but
    // fingerprint_snapshot strips those, so the install must be skipped.
    let installed_again = store.refresh_once().await;
    assert!(
        !installed_again,
        "second identical refresh should have been deduped"
    );
}

#[tokio::test]
async fn pause_agent_records_action_without_gt_binary() {
    let store = SnapshotStore::new(isolated_root());
    let action = store
        .pause_agent("gtui/polecats/ghost")
        .await
        .expect("action record returned");
    assert_eq!(action["kind"], "pause-agent");
    assert_eq!(action["target"], "gtui/polecats/ghost");
    // Without a real `gt` binary the subprocess fails — the store records
    // the failure rather than propagating it.
    assert_eq!(action["ok"], false);

    // The action must have been appended to the store's history.
    let history = store.action_history();
    assert!(!history.is_empty(), "history should have our action");
    assert_eq!(history[0]["kind"], "pause-agent");
}

#[tokio::test]
async fn inject_instruction_records_action_and_rejects_blank() {
    let store = SnapshotStore::new(isolated_root());
    let err = store
        .inject_instruction("gtui/polecats/nux", "")
        .await
        .expect_err("blank should fail");
    assert!(err.contains("empty"));

    let action = store
        .inject_instruction("gtui/polecats/nux", "pause after the next step")
        .await
        .expect("non-blank recorded");
    assert_eq!(action["kind"], "inject-instruction");
    assert_eq!(action["target"], "gtui/polecats/nux");
}

#[tokio::test]
async fn snapshot_exposes_latest_action_history() {
    let store = SnapshotStore::new(isolated_root());
    for i in 0..(SNAPSHOT_ACTION_LIMIT + 5) {
        store.record_action(json!({"seq": i}));
    }
    // Push a refresh so the snapshot picks up the new history window.
    let _ = store.refresh_once().await;
    let snap = store.get();
    assert_eq!(snap.actions.len(), SNAPSHOT_ACTION_LIMIT);
    // Newest first.
    assert_eq!(
        snap.actions[0]["seq"],
        (SNAPSHOT_ACTION_LIMIT + 4) as i64,
        "snapshot actions window should start at the newest entry"
    );
    assert!(store.action_history().len() <= ACTION_HISTORY_LIMIT);
}

#[tokio::test]
async fn write_terminal_validates_target_and_pane() {
    let store = SnapshotStore::new(isolated_root());

    // Unknown target
    let err = store
        .write_terminal("gtui/polecats/ghost", "hello")
        .await
        .expect_err("should error");
    assert!(err.contains("Unknown terminal target"), "got: {err}");

    // Known but session-less target
    let snap = WorkspaceSnapshot {
        agents: vec![gtui_lib::models::AgentInfo {
            target: "gtui/polecats/dormant".into(),
            has_session: false,
            ..Default::default()
        }],
        ..WorkspaceSnapshot::default()
    };
    store.install_snapshot(snap);
    let err = store
        .write_terminal("gtui/polecats/dormant", "hi")
        .await
        .expect_err("should error");
    assert!(err.contains("does not currently have"), "got: {err}");
}

#[tokio::test]
async fn fetch_diff_wires_through_unknown_repo_error() {
    let store = SnapshotStore::new(isolated_root());
    let err = store
        .fetch_diff("no-such-repo", "deadbeef")
        .await
        .expect_err("should error");
    assert!(err.contains("Unknown repo id"), "got: {err}");

    // Populate and try again — this will fail at the `git` call, but with a
    // different error message.
    let snap = WorkspaceSnapshot {
        git: json!({"repos": [{"id": "a", "root": "/nonexistent/path"}]}),
        ..WorkspaceSnapshot::default()
    };
    store.install_snapshot(snap);
    let err2 = store
        .fetch_diff("a", "deadbeef")
        .await
        .expect_err("should error");
    assert!(
        !err2.contains("Unknown repo id"),
        "expected a git failure now, got: {err2}"
    );
}

#[tokio::test]
async fn git_diff_cache_survives_multiple_inserts() {
    let store = SnapshotStore::new(isolated_root());
    for sha in ["abc", "def", "cafe"] {
        store.cache_git_diff(CachedGitDiff {
            repo_id: "gtui".into(),
            sha: sha.into(),
            text: format!("diff {sha}"),
            truncated: false,
        });
    }
    assert_eq!(
        store.get_cached_git_diff("gtui", "cafe").unwrap().text,
        "diff cafe"
    );
    assert!(store.get_cached_git_diff("gtui", "missing").is_none());
    store.clear_git_diff_cache();
    assert!(store.get_cached_git_diff("gtui", "abc").is_none());
}

#[test]
fn fingerprint_is_stable_across_identical_snapshots() {
    let a = WorkspaceSnapshot {
        alerts: vec!["same".into()],
        ..WorkspaceSnapshot::default()
    };
    let b = WorkspaceSnapshot {
        alerts: vec!["same".into()],
        ..WorkspaceSnapshot::default()
    };
    assert_eq!(fingerprint_snapshot(&a), fingerprint_snapshot(&b));
}

#[tokio::test]
async fn spawn_and_shutdown_cycle_is_bounded() {
    let store = SnapshotStore::with_interval(isolated_root(), Duration::from_millis(20));
    let handle = store.spawn();
    tokio::time::sleep(Duration::from_millis(60)).await;
    store.shutdown();
    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        result.is_ok(),
        "polling task must exit within 2s of shutdown"
    );
}

#[test]
fn fixture_dir_layout_is_intact() {
    // Guardrail: if someone renames/deletes a fixture the other tests will
    // fail late. This catches fixture drift in isolation.
    for name in [
        "gt_status_fast.txt",
        "gt_status_daemon_stopped.txt",
        "gt_feed.txt",
        "gt_crew_list.json",
        "codex_rollout.jsonl",
        "codex_rollout_turn_only.jsonl",
        "claude_session.jsonl",
        "claude_session_no_cwd.jsonl",
    ] {
        let text = common::load_fixture(name);
        assert!(!text.is_empty(), "fixture {name} is empty");
    }
}
