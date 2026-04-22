//! Parity assertions against the legacy `webui/server.py` snapshot fixtures
//! captured in gui-cqe.1.
//!
//! These tests pin the JSON shape and key counts that the Rust port of
//! `build_snapshot` must eventually reproduce. They run against the captured
//! fixtures (not against `build_snapshot`) so they pass today and will keep
//! defending the contract as downstream collectors come online.
//!
//! The goal is structural parity, not value equality — wall-clock timestamps
//! and live workspace state drift between captures, so we assert on shape and
//! presence rather than exact payloads.

mod common;

use std::collections::BTreeSet;

use common::load_fixture_json;
use serde_json::Value;

const POPULATED: &str = "webui_snapshot_populated.json";
const EMPTY: &str = "webui_snapshot_empty.json";

/// Top-level keys that `build_snapshot` returns on every frame, per the
/// legacy webui contract documented in `WEBUI_SNAPSHOT_PARITY.md`.
const TOP_LEVEL_KEYS: &[&str] = &[
    "actions",
    "activity",
    "agents",
    "alerts",
    "convoys",
    "crews",
    "errors",
    "generated_at",
    "generation_ms",
    "git",
    "graph",
    "gt_root",
    "status",
    "status_legend",
    "stores",
    "summary",
    "timings",
    "vitals_raw",
];

fn keys_of(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

fn assert_keys_present(value: &Value, expected: &[&str], context: &str) {
    let actual = keys_of(value);
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|k| !actual.contains(*k))
        .collect();
    assert!(
        missing.is_empty(),
        "{context}: missing keys {missing:?}\n  got: {actual:?}"
    );
}

fn array_len(value: &Value, path: &[&str]) -> usize {
    let mut cursor = value;
    for key in path {
        cursor = cursor
            .get(*key)
            .unwrap_or_else(|| panic!("path {:?} stopped at missing key {key}", path));
    }
    cursor
        .as_array()
        .unwrap_or_else(|| panic!("path {path:?} did not resolve to an array; got {cursor:?}"))
        .len()
}

fn object_len(value: &Value, path: &[&str]) -> usize {
    let mut cursor = value;
    for key in path {
        cursor = cursor
            .get(*key)
            .unwrap_or_else(|| panic!("path {:?} stopped at missing key {key}", path));
    }
    cursor
        .as_object()
        .unwrap_or_else(|| panic!("path {path:?} did not resolve to an object; got {cursor:?}"))
        .len()
}

#[test]
fn populated_fixture_has_every_top_level_key() {
    let snap = load_fixture_json(POPULATED);
    assert_keys_present(&snap, TOP_LEVEL_KEYS, "populated.<top-level>");
    assert_eq!(
        keys_of(&snap).len(),
        TOP_LEVEL_KEYS.len(),
        "populated fixture has unexpected top-level keys: {:?}",
        keys_of(&snap)
    );
}

#[test]
fn empty_fixture_has_every_top_level_key() {
    let snap = load_fixture_json(EMPTY);
    assert_keys_present(&snap, TOP_LEVEL_KEYS, "empty.<top-level>");
    assert_eq!(
        keys_of(&snap).len(),
        TOP_LEVEL_KEYS.len(),
        "empty fixture has unexpected top-level keys: {:?}",
        keys_of(&snap)
    );
}

#[test]
fn populated_fixture_graph_is_non_empty() {
    let snap = load_fixture_json(POPULATED);
    let nodes = array_len(&snap, &["graph", "nodes"]);
    let edges = array_len(&snap, &["graph", "edges"]);
    assert!(
        nodes > 0,
        "populated fixture should carry graph.nodes; got {nodes}"
    );
    assert!(
        edges > 0,
        "populated fixture should carry graph.edges; got {edges}"
    );
    // Captured fixture had 110/179. We assert "in the same ballpark" so
    // recapture under slightly different live state still passes — but a
    // truncated/zeroed graph would trip the lower bound.
    assert!(nodes >= 50, "graph.nodes shrank dramatically: {nodes}");
    assert!(edges >= 50, "graph.edges shrank dramatically: {edges}");
}

#[test]
fn empty_fixture_graph_is_zero() {
    let snap = load_fixture_json(EMPTY);
    assert_eq!(array_len(&snap, &["graph", "nodes"]), 0);
    assert_eq!(array_len(&snap, &["graph", "edges"]), 0);
}

#[test]
fn populated_fixture_activity_groups_present() {
    let snap = load_fixture_json(POPULATED);
    let groups = array_len(&snap, &["activity", "groups"]);
    let unassigned = array_len(&snap, &["activity", "unassigned_agents"]);
    assert!(
        groups > 0,
        "activity.groups should be non-empty in populated fixture"
    );
    assert!(
        unassigned > 0,
        "activity.unassigned_agents should list system agents (mayor/witness/etc)"
    );
}

#[test]
fn empty_fixture_activity_has_no_groups_but_keeps_system_agents() {
    let snap = load_fixture_json(EMPTY);
    assert_eq!(
        array_len(&snap, &["activity", "groups"]),
        0,
        "empty workspace should have zero activity groups"
    );
    // The `gt` CLI still returns globally-registered rigs even against an
    // empty root, so unassigned_agents stays non-empty.
    assert!(
        array_len(&snap, &["activity", "unassigned_agents"]) > 0,
        "system/global agents should still be reported on the empty fixture"
    );
}

#[test]
fn populated_fixture_git_section_is_populated() {
    let snap = load_fixture_json(POPULATED);
    let repos = array_len(&snap, &["git", "repos"]);
    let commits = array_len(&snap, &["git", "recent_commits"]);
    let task_memory = object_len(&snap, &["git", "task_memory"]);
    assert!(repos > 0, "git.repos should be non-empty");
    assert!(commits > 0, "git.recent_commits should be non-empty");
    assert!(
        task_memory > 0,
        "git.task_memory should map task ids → commit refs"
    );
}

#[test]
fn populated_fixture_repos_carry_expected_keys() {
    let snap = load_fixture_json(POPULATED);
    let first_repo = &snap["git"]["repos"][0];
    assert_keys_present(
        first_repo,
        &[
            "branches",
            "id",
            "label",
            "recent_commits",
            "root",
            "scope",
            "scopes",
            "status",
            "worktrees",
        ],
        "git.repos[0]",
    );
    assert!(
        first_repo["id"].as_str().is_some_and(|s| !s.is_empty()),
        "git.repos[0].id must be a non-empty string"
    );
    assert!(
        first_repo["root"].as_str().is_some_and(|s| !s.is_empty()),
        "git.repos[0].root must be a non-empty string"
    );
}

#[test]
fn populated_fixture_recent_commits_carry_expected_keys() {
    let snap = load_fixture_json(POPULATED);
    let commit = &snap["git"]["recent_commits"][0];
    assert_keys_present(
        commit,
        &[
            "committed_at",
            "refs",
            "repo_id",
            "repo_label",
            "sha",
            "short_sha",
            "subject",
            "task_ids",
        ],
        "git.recent_commits[0]",
    );
    assert!(
        commit["sha"].as_str().is_some_and(|s| s.len() >= 7),
        "recent_commits[0].sha must look like a git sha"
    );
}

#[test]
fn populated_fixture_links_at_least_one_commit_to_a_task() {
    // Any commit with a task id, OR any graph node with linked_commits, counts.
    let snap = load_fixture_json(POPULATED);
    let any_commit_linked = snap["git"]["recent_commits"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|c| {
                c.get("task_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| !ids.is_empty())
            })
        })
        .unwrap_or(false);

    let any_node_linked = snap["graph"]["nodes"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|n| {
                n.get("linked_commit_count")
                    .and_then(Value::as_u64)
                    .is_some_and(|c| c > 0)
            })
        })
        .unwrap_or(false);

    assert!(
        any_commit_linked || any_node_linked,
        "expected at least one commit↔task link in the populated fixture; \
         neither recent_commits[*].task_ids nor graph.nodes[*].linked_commit_count had any hits"
    );
}

#[test]
fn populated_fixture_convoys_present_and_indexed() {
    let snap = load_fixture_json(POPULATED);
    let convoys = array_len(&snap, &["convoys", "convoys"]);
    let task_index = object_len(&snap, &["convoys", "task_index"]);
    assert!(convoys > 0, "convoys.convoys should be non-empty");
    assert!(
        task_index > 0,
        "convoys.task_index should be keyed by task id"
    );
    let first = &snap["convoys"]["convoys"][0];
    assert_keys_present(
        first,
        &["completed", "id", "status", "title", "total", "tracked_ids"],
        "convoys.convoys[0]",
    );
}

#[test]
fn empty_fixture_convoys_are_zero() {
    let snap = load_fixture_json(EMPTY);
    assert_eq!(array_len(&snap, &["convoys", "convoys"]), 0);
}

#[test]
fn populated_fixture_stores_carry_expected_shape() {
    let snap = load_fixture_json(POPULATED);
    let count = array_len(&snap, &["stores"]);
    assert!(count > 0, "stores should be non-empty in populated fixture");
    let store = &snap["stores"][0];
    assert_keys_present(
        store,
        &[
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
        ],
        "stores[0]",
    );
}

#[test]
fn empty_fixture_stores_are_zero() {
    let snap = load_fixture_json(EMPTY);
    assert_eq!(array_len(&snap, &["stores"]), 0);
}

#[test]
fn populated_fixture_crews_carry_enrichment_and_running_metadata() {
    let snap = load_fixture_json(POPULATED);
    let crews = snap["crews"].as_array().expect("crews must be an array");
    assert!(!crews.is_empty(), "populated fixture should list crews");

    // Every crew row is expected to carry merged running-state metadata
    // (branch/has_session/mail_*) alongside the enrichment fields added by
    // `enrich_crew_workspace`. The Rust port of `merge_crews` must preserve
    // this contract.
    for (i, crew) in crews.iter().enumerate() {
        assert_keys_present(
            crew,
            &[
                "branch",
                "git_benign_modified",
                "git_benign_untracked",
                "git_has_local_state_only",
                "git_has_risky_changes",
                "git_modified",
                "git_risky_modified",
                "git_risky_untracked",
                "git_state",
                "git_status_label",
                "git_status_tone",
                "git_untracked",
                "has_session",
                "name",
                "path",
                "rig",
            ],
            &format!("crews[{i}]"),
        );
    }

    // Ordering contract: crews sort by (rig, name).
    let keys: Vec<(String, String)> = crews
        .iter()
        .map(|c| {
            (
                c["rig"].as_str().unwrap_or("").to_string(),
                c["name"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "crews must be sorted by (rig, name)");

    // The partition invariant: modified = benign_modified + risky_modified
    // (same for untracked). Any captured crew must satisfy it.
    for (i, crew) in crews.iter().enumerate() {
        let modified = array_len(crew, &["git_modified"]);
        let benign = array_len(crew, &["git_benign_modified"]);
        let risky = array_len(crew, &["git_risky_modified"]);
        assert_eq!(
            modified,
            benign + risky,
            "crews[{i}].git_modified must equal git_benign_modified + git_risky_modified"
        );
        let untracked = array_len(crew, &["git_untracked"]);
        let benign_u = array_len(crew, &["git_benign_untracked"]);
        let risky_u = array_len(crew, &["git_risky_untracked"]);
        assert_eq!(
            untracked,
            benign_u + risky_u,
            "crews[{i}].git_untracked must equal git_benign_untracked + git_risky_untracked"
        );
    }
}

#[test]
fn populated_fixture_agents_carry_expected_keys() {
    let snap = load_fixture_json(POPULATED);
    let agents = snap["agents"].as_array().expect("agents must be an array");
    assert!(!agents.is_empty(), "populated fixture should list agents");
    assert_keys_present(
        &agents[0],
        &[
            "crew",
            "current_command",
            "current_path",
            "events",
            "has_session",
            "hook",
            "kind",
            "label",
            "pane_id",
            "recent_task",
            "role",
            "scope",
            "session_name",
            "target",
            "task_events",
        ],
        "agents[0]",
    );
}

#[test]
fn graph_node_inner_keys_match_python() {
    let snap = load_fixture_json(POPULATED);
    let node = &snap["graph"]["nodes"][0];
    assert_keys_present(
        node,
        &[
            "agent_targets",
            "assignee",
            "blocked_by",
            "blocked_by_count",
            "closed_at",
            "created_at",
            "dependency_count",
            "dependent_count",
            "description",
            "id",
            "is_system",
            "kind",
            "labels",
            "linked_commit_count",
            "linked_commits",
            "owner",
            "parent",
            "priority",
            "scope",
            "status",
            "title",
            "type",
            "ui_status",
            "updated_at",
        ],
        "graph.nodes[0]",
    );
}

#[test]
fn graph_edge_keys_are_minimal_and_stable() {
    let snap = load_fixture_json(POPULATED);
    let edge = &snap["graph"]["edges"][0];
    let actual = keys_of(edge);
    let expected: BTreeSet<String> = ["kind", "source", "target"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        actual, expected,
        "graph.edges[0] should expose exactly {{kind, source, target}}"
    );
}

#[test]
fn activity_group_inner_keys_match_python() {
    let snap = load_fixture_json(POPULATED);
    let group = &snap["activity"]["groups"][0];
    assert_keys_present(
        group,
        &[
            "agent_count",
            "agents",
            "events",
            "is_system",
            "memory",
            "scope",
            "stored_status",
            "task_id",
            "title",
            "ui_status",
        ],
        "activity.groups[0]",
    );
}

#[test]
fn summary_carries_all_counter_fields() {
    let expected = &[
        "active_agents",
        "command_errors",
        "derived_status_counts",
        "done_tasks",
        "ready_tasks",
        "repos",
        "running_tasks",
        "stored_status_counts",
        "stuck_tasks",
        "system_running",
        "task_groups",
    ];
    for fixture in [POPULATED, EMPTY] {
        let snap = load_fixture_json(fixture);
        assert_keys_present(&snap["summary"], expected, &format!("{fixture}.summary"));
    }
}

#[test]
fn populated_summary_counters_are_internally_consistent() {
    let snap = load_fixture_json(POPULATED);
    let summary = &snap["summary"];

    // command_errors should match errors.len() — that's how `build_snapshot`
    // wires it (and the current Rust port already enforces this, so the
    // fixture must agree).
    let errors = array_len(&snap, &["errors"]);
    let counter = summary["command_errors"].as_u64().unwrap_or(u64::MAX) as usize;
    assert_eq!(
        counter, errors,
        "summary.command_errors must equal errors.len()"
    );

    // task_groups should match activity.groups.len().
    let groups = array_len(&snap, &["activity", "groups"]) as u64;
    assert_eq!(
        summary["task_groups"].as_u64(),
        Some(groups),
        "summary.task_groups must equal activity.groups.len()"
    );

    // repos should match git.repos.len().
    let repos = array_len(&snap, &["git", "repos"]) as u64;
    assert_eq!(
        summary["repos"].as_u64(),
        Some(repos),
        "summary.repos must equal git.repos.len()"
    );

    // active_agents ≤ agents.len() (only the live ones contribute).
    let agents_total = array_len(&snap, &["agents"]) as u64;
    let active = summary["active_agents"].as_u64().expect("u64");
    assert!(
        active <= agents_total,
        "summary.active_agents ({active}) cannot exceed agents.len() ({agents_total})"
    );
}

#[test]
fn status_legend_has_seven_entries_in_both_fixtures() {
    for fixture in [POPULATED, EMPTY] {
        let snap = load_fixture_json(fixture);
        assert_eq!(
            array_len(&snap, &["status_legend"]),
            7,
            "{fixture}.status_legend should have 7 entries (matches default_status_legend)"
        );
    }
}

#[test]
fn status_inner_keys_match_python() {
    let snap = load_fixture_json(POPULATED);
    assert_keys_present(
        &snap["status"],
        &[
            "overseer",
            "raw",
            "root_path",
            "services",
            "tmux_socket",
            "town",
        ],
        "status",
    );
}

#[test]
fn alerts_are_strings_in_both_fixtures() {
    for fixture in [POPULATED, EMPTY] {
        let snap = load_fixture_json(fixture);
        let alerts = snap["alerts"].as_array().expect("alerts must be an array");
        for (i, alert) in alerts.iter().enumerate() {
            assert!(
                alert.is_string(),
                "{fixture}.alerts[{i}] must be a string, got {alert:?}"
            );
        }
    }
}

#[test]
fn populated_actions_capped_at_twelve() {
    let snap = load_fixture_json(POPULATED);
    let count = array_len(&snap, &["actions"]);
    assert!(
        count <= 12,
        "snapshot.actions must respect SNAPSHOT_ACTION_LIMIT=12; got {count}"
    );
}

#[test]
fn empty_fixture_actions_are_zero() {
    let snap = load_fixture_json(EMPTY);
    assert_eq!(array_len(&snap, &["actions"]), 0);
}
