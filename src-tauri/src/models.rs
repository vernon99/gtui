//! Data model for the workspace snapshot exchanged between the Rust backend
//! and the WebView frontend.
//!
//! Field names and JSON shape match the existing Python implementation
//! (`webui/server.py`) so the JS frontend can consume either backend without
//! code changes while the migration is in flight. Where the Python code
//! uses loose `dict[str, Any]` for fields that haven't been schematised yet
//! (graph, git, convoys, etc.) we preserve the same flexibility here with
//! `serde_json::Value`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Parsed shape of `gt status --fast` output.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusSummary {
    #[serde(default)]
    pub town: String,
    #[serde(default)]
    pub root_path: String,
    #[serde(default)]
    pub overseer: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub tmux_socket: String,
    #[serde(default)]
    pub raw: String,
}

/// A rig as reported by `rigs.json` / `gt crew list`. Fields beyond the
/// identifier are sourced from the crew listing and kept loose for now.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct RigInfo {
    pub name: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub path: String,
    #[serde(default, flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A single task node (issue) as rendered for the graph / activity panes.
///
/// Matches `compact_issue()` in `webui/server.py` plus the `kind`, `scope`,
/// `agent_targets`, `ui_status`, `is_system` fields that `finalize_graph()`
/// layers on top.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub ui_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(rename = "type", default)]
    pub issue_type: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub assignee: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub dependency_count: u32,
    #[serde(default)]
    pub dependent_count: u32,
    #[serde(default)]
    pub blocked_by_count: u32,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub is_system: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub agent_targets: Vec<String>,
}

/// Lightweight agent payload used for both the activity pane and the
/// unassigned-agents list. Matches the dictionary assembled in
/// `build_activity_groups()` but with typed common fields.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentInfo {
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub has_session: bool,
    #[serde(default)]
    pub runtime_state: String,
    #[serde(default)]
    pub current_path: String,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub current_command: String,
    #[serde(default)]
    pub hook: Value,
    #[serde(default)]
    pub events: Vec<Value>,
    #[serde(default)]
    pub task_events: Vec<Value>,
    #[serde(default)]
    pub recent_task: Value,
    #[serde(default)]
    pub crew: Value,
    #[serde(default)]
    pub polecat: Value,
}

/// A task-centric activity group: one bead plus every agent attached to it.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityGroup {
    pub task_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub stored_status: String,
    #[serde(default)]
    pub ui_status: String,
    #[serde(default)]
    pub is_system: bool,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub events: Vec<Value>,
    #[serde(default)]
    pub memory: Vec<Value>,
    #[serde(default)]
    pub agent_count: u32,
}

/// Container for the `activity` field of the snapshot.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Activity {
    #[serde(default)]
    pub groups: Vec<ActivityGroup>,
    #[serde(default)]
    pub unassigned_agents: Vec<AgentInfo>,
}

/// Aggregate counters surfaced at the top of the UI.
///
/// Mirrors `summary` in `build_snapshot()`. Grouped JSON sub-objects
/// (`stored_status_counts`, `derived_status_counts`) keep the Python shape.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Metrics {
    #[serde(default)]
    pub running_tasks: u32,
    #[serde(default)]
    pub stuck_tasks: u32,
    #[serde(default)]
    pub ready_tasks: u32,
    #[serde(default)]
    pub done_tasks: u32,
    #[serde(default)]
    pub system_running: u32,
    #[serde(default)]
    pub active_agents: u32,
    #[serde(default)]
    pub task_groups: u32,
    #[serde(default)]
    pub repos: u32,
    #[serde(default)]
    pub command_errors: u32,
    #[serde(default)]
    pub stored_status_counts: BTreeMap<String, u32>,
    #[serde(default)]
    pub derived_status_counts: BTreeMap<String, u32>,
}

/// Per-phase timings recorded while building a snapshot.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Timings {
    #[serde(default)]
    pub gt_commands_ms: u64,
    #[serde(default)]
    pub agent_commands_ms: u64,
    #[serde(default)]
    pub bd_commands_ms: u64,
    #[serde(default)]
    pub git_commands_ms: u64,
    #[serde(default)]
    pub convoy_commands_ms: u64,
}

/// Serialized subprocess failure surfaced in `snapshot.errors`. Produced by
/// `CommandResult::to_error`; kept as `Value` on the snapshot itself because
/// some entries carry extra adhoc fields and renaming them all would cost us
/// backend/frontend parity during the port.
pub type CommandError = Value;

/// Entry in the snapshot's legend describing a single bead status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusLegendEntry {
    pub name: String,
    pub icon: String,
    pub category: String,
    pub meaning: String,
}

/// Top-level snapshot: everything the UI needs to render one frame of the
/// dashboard. Fields that are still loosely typed in the Python port
/// (`graph`, `git`, `convoys`, `crews`, `stores`, `actions`) are kept as
/// `serde_json::Value` so we can wire Tauri IPC without finalising their
/// schema this round.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceSnapshot {
    pub generated_at: String,
    #[serde(default)]
    pub generation_ms: u64,
    #[serde(default)]
    pub gt_root: String,
    #[serde(default)]
    pub status: StatusSummary,
    #[serde(default)]
    pub vitals_raw: String,
    #[serde(default)]
    pub status_legend: Vec<StatusLegendEntry>,
    #[serde(default)]
    pub summary: Metrics,
    #[serde(default)]
    pub alerts: Vec<String>,
    #[serde(default)]
    pub graph: Value,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub git: Value,
    #[serde(default)]
    pub convoys: Value,
    #[serde(default)]
    pub crews: Vec<Value>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default)]
    pub stores: Vec<Value>,
    #[serde(default)]
    pub actions: Vec<Value>,
    #[serde(default)]
    pub errors: Vec<CommandError>,
    #[serde(default)]
    pub timings: Timings,
}

/// The canonical legend. Matches `STATUS_LEGEND` in `webui/server.py`.
pub fn default_status_legend() -> Vec<StatusLegendEntry> {
    vec![
        StatusLegendEntry {
            name: "open".into(),
            icon: "○".into(),
            category: "active".into(),
            meaning: "Available to work (default)".into(),
        },
        StatusLegendEntry {
            name: "in_progress".into(),
            icon: "◐".into(),
            category: "wip".into(),
            meaning: "Actively being worked on".into(),
        },
        StatusLegendEntry {
            name: "blocked".into(),
            icon: "●".into(),
            category: "wip".into(),
            meaning: "Blocked by a dependency".into(),
        },
        StatusLegendEntry {
            name: "deferred".into(),
            icon: "❄".into(),
            category: "frozen".into(),
            meaning: "Deliberately put on ice for later".into(),
        },
        StatusLegendEntry {
            name: "closed".into(),
            icon: "✓".into(),
            category: "done".into(),
            meaning: "Completed".into(),
        },
        StatusLegendEntry {
            name: "pinned".into(),
            icon: "📌".into(),
            category: "frozen".into(),
            meaning: "Persistent, stays open indefinitely".into(),
        },
        StatusLegendEntry {
            name: "hooked".into(),
            icon: "◇".into(),
            category: "wip".into(),
            meaning: "Attached to an agent hook".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_snapshot_round_trips() {
        let snap = WorkspaceSnapshot {
            generated_at: "2026-04-21T21:55:00-07:00".into(),
            generation_ms: 123,
            gt_root: "/home/user/gt".into(),
            status: StatusSummary {
                town: "gastown".into(),
                root_path: "/home/user/gt".into(),
                overseer: "mayor".into(),
                services: vec!["daemon (running)".into()],
                tmux_socket: "gastown".into(),
                raw: "Town: gastown\n".into(),
            },
            vitals_raw: "ok".into(),
            status_legend: default_status_legend(),
            summary: Metrics {
                running_tasks: 3,
                active_agents: 2,
                task_groups: 4,
                stored_status_counts: BTreeMap::from([("open".into(), 5)]),
                derived_status_counts: BTreeMap::from([("running".into(), 3)]),
                ..Metrics::default()
            },
            alerts: vec!["Gas Town daemon is stopped.".into()],
            graph: json!({"nodes": [], "edges": []}),
            activity: Activity {
                groups: vec![ActivityGroup {
                    task_id: "gui-bn8.3".into(),
                    title: "Port data models".into(),
                    ui_status: "running".into(),
                    agents: vec![AgentInfo {
                        target: "gtui/polecats/furiosa".into(),
                        role: "polecat".into(),
                        has_session: true,
                        ..AgentInfo::default()
                    }],
                    agent_count: 1,
                    ..ActivityGroup::default()
                }],
                unassigned_agents: vec![],
            },
            git: json!({"repos": []}),
            convoys: json!({"convoys": []}),
            crews: vec![],
            agents: vec![],
            stores: vec![],
            actions: vec![],
            errors: vec![],
            timings: Timings {
                gt_commands_ms: 123,
                ..Timings::default()
            },
        };

        let encoded = serde_json::to_string(&snap).expect("encode");
        let decoded: WorkspaceSnapshot = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(snap, decoded);
    }

    #[test]
    fn task_info_preserves_python_field_names() {
        let task = TaskInfo {
            id: "gui-bn8.3".into(),
            title: "Port data models".into(),
            issue_type: "task".into(),
            priority: Some(1),
            labels: vec!["polecat".into()],
            agent_targets: vec!["gtui/polecats/furiosa".into()],
            ..TaskInfo::default()
        };
        let value: Value = serde_json::to_value(&task).expect("encode");
        assert_eq!(value["type"], "task");
        assert_eq!(value["priority"], 1);
        assert_eq!(value["agent_targets"][0], "gtui/polecats/furiosa");
        let back: TaskInfo = serde_json::from_value(value).expect("decode");
        assert_eq!(back, task);
    }

    #[test]
    fn status_legend_matches_python_count_and_order() {
        let legend = default_status_legend();
        assert_eq!(legend.len(), 7);
        assert_eq!(legend[0].name, "open");
        assert_eq!(legend[6].name, "hooked");
    }

    #[test]
    fn activity_group_accepts_missing_optional_fields() {
        let raw = json!({
            "task_id": "gui-bn8.3",
            "title": "port models",
        });
        let group: ActivityGroup = serde_json::from_value(raw).expect("decode");
        assert_eq!(group.task_id, "gui-bn8.3");
        assert_eq!(group.title, "port models");
        assert_eq!(group.agents.len(), 0);
    }

    #[test]
    fn rig_info_captures_unknown_fields() {
        let raw = json!({
            "name": "gtui",
            "scope": "gtui",
            "path": "/home/user/gt/gtui",
            "custom_field": 42,
        });
        let rig: RigInfo = serde_json::from_value(raw).expect("decode");
        assert_eq!(rig.name, "gtui");
        assert_eq!(rig.extra.get("custom_field"), Some(&Value::from(42)));
    }
}
