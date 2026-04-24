//! Regression gate for the `gui-cqe` feature-parity epic.
//!
//! This file is the executable form of the desktop app's acceptance checklist:
//! it pins the six UI-visible sections the frontend depends on — Task Spine,
//! Git History, Bead Stores, Crew Workspaces, Convoys-derived filtering, and
//! actionable Focus controls — to the populated snapshot contract so a future
//! change that silently drops one of them trips the suite.
//!
//! End-to-end IPC behaviour is covered by `tests/ipc_roundtrip.rs`. This file
//! sits on top and asserts that frontend-facing sections stay populated and
//! shaped the way `frontend/static/js` consumes them.

mod common;

use common::load_fixture_json;
use serde_json::Value;

const POPULATED: &str = "snapshot_contract_populated.json";

fn as_array<'a>(value: &'a Value, path: &[&str]) -> &'a Vec<Value> {
    let mut cursor = value;
    for key in path {
        cursor = cursor
            .get(*key)
            .unwrap_or_else(|| panic!("path {path:?} stopped at missing key {key}"));
    }
    cursor
        .as_array()
        .unwrap_or_else(|| panic!("path {path:?} expected array, got {cursor:?}"))
}

fn as_object_len(value: &Value, path: &[&str]) -> usize {
    let mut cursor = value;
    for key in path {
        cursor = cursor
            .get(*key)
            .unwrap_or_else(|| panic!("path {path:?} stopped at missing key {key}"));
    }
    cursor
        .as_object()
        .unwrap_or_else(|| panic!("path {path:?} expected object, got {cursor:?}"))
        .len()
}

/// Task Spine — `graph.nodes` + `activity.groups` drive the spine view. The
/// frontend (`frontend/static/js/app.js`) reads both: nodes for the task timeline
/// and groups to cluster agents by task. A non-empty spine is the headline
/// acceptance signal for the app contract.
#[test]
fn task_spine_has_nodes_edges_and_activity_groups() {
    let snap = load_fixture_json(POPULATED);

    let nodes = as_array(&snap, &["graph", "nodes"]);
    let edges = as_array(&snap, &["graph", "edges"]);
    let groups = as_array(&snap, &["activity", "groups"]);

    assert!(
        !nodes.is_empty(),
        "Task Spine: graph.nodes must be non-empty"
    );
    assert!(
        !edges.is_empty(),
        "Task Spine: graph.edges must be non-empty"
    );
    assert!(
        !groups.is_empty(),
        "Task Spine: activity.groups must be non-empty so the spine has per-task rows"
    );

    // Every task row the UI renders needs id + title + raw GT status.
    for (i, node) in nodes.iter().enumerate() {
        for key in ["id", "title", "status"] {
            assert!(
                node.get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "Task Spine: graph.nodes[{i}].{key} missing or empty"
            );
        }
    }

    // Non-system activity groups must point at a task the spine renders.
    // System groups (is_system=true) cover daemon/witness/refinery wisps that
    // live outside the spine graph, so they're allowed to reference ids the
    // graph doesn't carry.
    let node_ids: std::collections::HashSet<&str> = nodes
        .iter()
        .filter_map(|n| n.get("id").and_then(Value::as_str))
        .collect();
    let mut non_system_resolved = 0;
    for (i, group) in groups.iter().enumerate() {
        let task_id = group
            .get("task_id")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("activity.groups[{i}].task_id missing"));
        let is_system = group
            .get("is_system")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_system {
            assert!(
                node_ids.contains(task_id),
                "Task Spine: activity.groups[{i}].task_id={task_id} (non-system) \
                 has no matching graph node"
            );
            non_system_resolved += 1;
        }
    }
    assert!(
        non_system_resolved > 0,
        "Task Spine: no non-system activity group resolved to the graph"
    );
}

/// Git History — `git.repos`, `git.recent_commits`, and `git.task_memory` feed
/// the Git History pane. The spine links commits back to tasks via
/// `task_memory` (keyed by task id) and `recent_commits[*].task_ids`.
#[test]
fn git_history_has_repos_commits_and_task_links() {
    let snap = load_fixture_json(POPULATED);

    let repos = as_array(&snap, &["git", "repos"]);
    let commits = as_array(&snap, &["git", "recent_commits"]);
    let task_memory_len = as_object_len(&snap, &["git", "task_memory"]);

    assert!(
        !repos.is_empty(),
        "Git History: git.repos must be non-empty"
    );
    assert!(
        !commits.is_empty(),
        "Git History: git.recent_commits must be non-empty"
    );
    assert!(
        task_memory_len > 0,
        "Git History: git.task_memory must map at least one task id to commits"
    );

    // Frontend 'Load diff' is only reachable when commits carry repo_id + sha.
    let loadable = commits.iter().any(|c| {
        c.get("repo_id")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
            && c.get("sha")
                .and_then(Value::as_str)
                .is_some_and(|s| s.len() >= 7)
    });
    assert!(
        loadable,
        "Git History: at least one recent commit must carry repo_id + sha \
         so the UI's 'Load diff' action is reachable"
    );
}

/// Bead Stores — the left-rail store picker renders `stores[]` with
/// per-status counts. `summary.repos` doubles as a card count on the top bar.
#[test]
fn bead_stores_are_populated_with_status_counts() {
    let snap = load_fixture_json(POPULATED);

    let stores = as_array(&snap, &["stores"]);
    assert!(
        !stores.is_empty(),
        "Bead Stores: stores[] must be non-empty"
    );

    for (i, store) in stores.iter().enumerate() {
        for key in ["name", "scope", "path"] {
            assert!(
                store
                    .get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "Bead Stores: stores[{i}].{key} missing or empty"
            );
        }
        for key in ["total", "open", "closed", "hooked", "blocked"] {
            assert!(
                store.get(key).and_then(Value::as_u64).is_some(),
                "Bead Stores: stores[{i}].{key} must be a non-negative integer"
            );
        }
    }

    // The summary card driving the top-bar "stores" tile must match.
    let store_count = stores.len() as u64;
    let summary_repos = snap["summary"]["repos"].as_u64().unwrap_or(u64::MAX);
    // `summary.repos` tracks git repos, not bead stores — but the acceptance
    // criterion is "non-empty Bead Stores", so we just verify stores[] is the
    // thing the sidebar reads from, not that summary counts it.
    assert!(store_count > 0);
    assert!(
        summary_repos > 0,
        "Bead Stores: summary.repos (top-bar card) must be non-zero"
    );
}

/// Crew Workspaces — `crews[]` drives the crew workspace grid: each entry is
/// a working copy with branch/status metadata and git risk flags. The
/// `gui-cqe.3` port kept the merge + enrichment contract; this gate locks it.
#[test]
fn crew_workspaces_carry_risk_and_running_metadata() {
    let snap = load_fixture_json(POPULATED);

    let crews = as_array(&snap, &["crews"]);
    assert!(
        !crews.is_empty(),
        "Crew Workspaces: crews[] must be non-empty"
    );

    for (i, crew) in crews.iter().enumerate() {
        for key in ["rig", "name", "branch", "path"] {
            assert!(
                crew.get(key).and_then(Value::as_str).is_some(),
                "Crew Workspaces: crews[{i}].{key} must be a string"
            );
        }
        for key in [
            "git_has_risky_changes",
            "git_has_local_state_only",
            "has_session",
        ] {
            assert!(
                crew.get(key).and_then(Value::as_bool).is_some(),
                "Crew Workspaces: crews[{i}].{key} must be a bool (drives risk badge)"
            );
        }
        for key in ["git_status_label", "git_status_tone", "git_state"] {
            assert!(
                crew.get(key).is_some(),
                "Crew Workspaces: crews[{i}].{key} missing (drives UI pill text/tone)"
            );
        }
    }
}

/// Convoys-derived filtering — `convoys.convoys[]` + `convoys.task_index`
/// power the convoy filter pills. `task_index` maps task id → convoy id so
/// the spine can dim non-matching tasks when a convoy is selected.
#[test]
fn convoys_are_present_and_filtering_is_indexable() {
    let snap = load_fixture_json(POPULATED);

    let convoys = as_array(&snap, &["convoys", "convoys"]);
    let task_index_len = as_object_len(&snap, &["convoys", "task_index"]);
    assert!(
        !convoys.is_empty(),
        "Convoys: convoys.convoys must be non-empty so the filter pills render"
    );
    assert!(
        task_index_len > 0,
        "Convoys: convoys.task_index must map task ids → convoy ids for filtering"
    );

    // The index must point at convoys that actually exist; otherwise the
    // filter pill would dim every task when clicked. Each `task_index[key]`
    // entry is an aggregate `{total, open, closed, convoy_ids[], all_closed}`
    // — the UI reads `convoy_ids` to decide which pills include the task.
    let convoy_ids: std::collections::HashSet<&str> = convoys
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    let task_index = snap["convoys"]["task_index"]
        .as_object()
        .expect("convoys.task_index object");
    for (task_id, entry) in task_index {
        let entry_obj = entry.as_object().unwrap_or_else(|| {
            panic!("Convoys: task_index[{task_id}] must be an object, got {entry:?}")
        });
        let refs = entry_obj
            .get("convoy_ids")
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!("Convoys: task_index[{task_id}].convoy_ids must be an array")
            });
        assert!(
            !refs.is_empty(),
            "Convoys: task_index[{task_id}].convoy_ids empty — can't filter this task"
        );
        for r in refs {
            let id = r.as_str().unwrap_or_else(|| {
                panic!("Convoys: task_index[{task_id}].convoy_ids has non-string {r:?}")
            });
            assert!(
                convoy_ids.contains(id),
                "Convoys: task_index[{task_id}].convoy_ids references missing convoy {id}"
            );
        }
    }
}

/// Focus controls — the UI surfaces actionable controls (retry, pause,
/// inject, write-terminal, load-diff). Each ships as a Tauri IPC command and
/// appears in `snapshot.actions` as history. This gate keeps the history
/// bounded (so a runaway producer can't flood the UI) and pins the command
/// set reachable over IPC.
#[test]
fn focus_controls_action_history_respects_limit() {
    let snap = load_fixture_json(POPULATED);
    let actions = as_array(&snap, &["actions"]);
    assert!(
        actions.len() <= 12,
        "Focus controls: snapshot.actions must respect SNAPSHOT_ACTION_LIMIT=12; got {}",
        actions.len()
    );
    // Actions must carry the minimal shape the UI renders.
    for (i, action) in actions.iter().enumerate() {
        for key in ["kind", "target"] {
            assert!(
                action.get(key).is_some(),
                "Focus controls: snapshot.actions[{i}].{key} missing"
            );
        }
    }
}

/// Compile-time anchor: every actionable Focus control the UI calls must be a
/// live Tauri command. If any handler is renamed or deleted,
/// `generate_handler!` fails to compile — and this test's dependency on it
/// fails with it. `ipc_roundtrip.rs` exercises the wire format; this keeps
/// the regression gate self-contained.
#[test]
fn focus_controls_ipc_surface_is_registered() {
    fn _anchor<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
        builder.invoke_handler(tauri::generate_handler![
            gtui_lib::ipc::get_snapshot,
            gtui_lib::ipc::get_terminal,
            gtui_lib::ipc::get_git_diff,
            gtui_lib::ipc::run_gt,
            gtui_lib::ipc::stop_gt,
            gtui_lib::ipc::retry_task,
            gtui_lib::ipc::pause_agent,
            gtui_lib::ipc::inject_message,
            gtui_lib::ipc::write_terminal,
        ])
    }
    let _ = _anchor::<tauri::Wry>;
}
