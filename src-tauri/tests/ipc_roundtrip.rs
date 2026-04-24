//! End-to-end tests for the Tauri IPC surface.
//!
//! Unit tests in `src/ipc.rs` call each handler function directly; these
//! tests instead build a mock Tauri app, register the real `invoke_handler`,
//! and dispatch JSON payloads through `get_ipc_response`. That path covers:
//!
//! * Command registration (all nine handlers reachable by name)
//! * Argument-name wire format (JS `taskId` deserialises to Rust `task_id`)
//! * The `Result<T, String>` <-> IPC `Ok`/`Err` envelope
//! * Managed `SnapshotStore` state retrieval
//!
//! The underlying store methods have their own unit coverage; the focus here
//! is the IPC boundary itself.

use std::path::PathBuf;

use gtui_lib::models::{AgentInfo, WorkspaceSnapshot};
use gtui_lib::snapshot::SnapshotStore;
use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody};
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{App, WebviewUrl, WebviewWindowBuilder};

fn isolated_root() -> PathBuf {
    std::env::temp_dir()
        .join("gtui-ipc-roundtrip")
        .join("missing-root")
}

fn build_app(store: SnapshotStore) -> App<tauri::test::MockRuntime> {
    mock_builder()
        .manage(store)
        .invoke_handler(tauri::generate_handler![
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
        .build(mock_context(noop_assets()))
        .expect("build mock app")
}

fn make_request(cmd: &str, body: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        url: "http://tauri.localhost".parse().unwrap(),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    }
}

fn invoke_json(
    webview: &tauri::WebviewWindow<tauri::test::MockRuntime>,
    cmd: &str,
    body: Value,
) -> Result<Value, Value> {
    get_ipc_response(webview, make_request(cmd, body)).map(|b| {
        b.deserialize::<Value>()
            .expect("response body deserialises as JSON")
    })
}

fn new_webview(
    app: &App<tauri::test::MockRuntime>,
) -> tauri::WebviewWindow<tauri::test::MockRuntime> {
    WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .build()
        .expect("build webview")
}

#[test]
fn get_snapshot_round_trips_installed_snapshot() {
    let store = SnapshotStore::new(isolated_root());
    let snap = WorkspaceSnapshot {
        generated_at: "2026-04-22T00:00:00Z".into(),
        gt_root: "/tmp/ipc-roundtrip".into(),
        alerts: vec!["ipc-ok".into()],
        ..WorkspaceSnapshot::default()
    };
    store.install_snapshot(snap);

    let app = build_app(store);
    let webview = new_webview(&app);

    let value = invoke_json(&webview, "get_snapshot", json!({})).expect("get_snapshot ok");
    assert_eq!(value["gt_root"], "/tmp/ipc-roundtrip");
    assert_eq!(value["alerts"][0], "ipc-ok");
}

#[test]
fn get_terminal_errors_on_unknown_target() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let err = invoke_json(&webview, "get_terminal", json!({"target": "gtui/ghost"}))
        .expect_err("unknown target must error");
    assert!(
        err.as_str()
            .unwrap_or("")
            .contains("Unknown terminal target"),
        "got: {err}"
    );
}

#[test]
fn get_git_diff_errors_on_unknown_repo() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let err = invoke_json(
        &webview,
        "get_git_diff",
        json!({"repo": "no-such-repo", "sha": "deadbeef"}),
    )
    .expect_err("unknown repo must error");
    assert!(
        err.as_str().unwrap_or("").contains("Unknown repo id"),
        "got: {err}"
    );
}

#[test]
fn run_gt_routes_over_ipc() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let value = invoke_json(&webview, "run_gt", json!({})).expect("run_gt returns action");
    assert_eq!(value["kind"], "run-gt");
    assert!(
        value["command"]
            .as_str()
            .unwrap_or("")
            .contains("gt up --restore --quiet"),
        "got: {}",
        value["command"]
    );
}

#[test]
fn stop_gt_routes_over_ipc() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let value = invoke_json(&webview, "stop_gt", json!({})).expect("stop_gt returns action");
    assert_eq!(value["kind"], "stop-gt");
    assert!(
        value["command"]
            .as_str()
            .unwrap_or("")
            .contains("gt down --polecats --quiet"),
        "got: {}",
        value["command"]
    );
}

#[test]
fn retry_task_camel_case_arg_reaches_rust_snake_case_param() {
    // Regression guard: the JS frontend posts `taskId` (camelCase); the Rust
    // handler declares `task_id` (snake_case). Tauri's default arg-rename
    // policy must keep the two wired up — if it ever changes, every retry
    // button in the UI would silently break.
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let err = invoke_json(&webview, "retry_task", json!({"taskId": "gui-missing"}))
        .expect_err("unknown task must error");
    assert!(
        err.as_str().unwrap_or("").contains("Unknown task"),
        "got: {err}"
    );

    // Passing the wrong case (snake_case) must fail argument decoding — not
    // mis-decode as an empty string and hit "Unknown task: ".
    let wrong_case = invoke_json(&webview, "retry_task", json!({"task_id": "gui-missing"}))
        .expect_err("snake_case arg should fail decoding");
    let msg = wrong_case.as_str().unwrap_or("").to_string();
    assert!(
        !msg.contains("Unknown task"),
        "snake_case arg must not decode as a missing task: {msg}"
    );
}

#[test]
fn retry_task_routes_hooked_status_to_unsling_branch() {
    let store = SnapshotStore::new(isolated_root());
    let snap = WorkspaceSnapshot {
        graph: json!({
            "nodes": [
                {
                    "id": "gui-hooked",
                    "status": "hooked",
                    "agent_targets": ["gtui/polecats/nux"],
                }
            ],
            "edges": [],
        }),
        ..WorkspaceSnapshot::default()
    };
    store.install_snapshot(snap);
    let app = build_app(store);
    let webview = new_webview(&app);

    // Without a real `gt` binary on the sandbox PATH, `gt unsling` fails and
    // the handler records the failure rather than propagating it as Err.
    let value = invoke_json(&webview, "retry_task", json!({"taskId": "gui-hooked"}))
        .expect("hooked task dispatches, records action");
    assert_eq!(value["kind"], "retry-task");
    assert_eq!(value["task_id"], "gui-hooked");
    assert!(
        value["command"]
            .as_str()
            .unwrap_or("")
            .contains("gt unsling"),
        "hooked tasks must route to unsling: {}",
        value["command"]
    );
}

#[test]
fn pause_agent_records_action_over_ipc() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let value = invoke_json(
        &webview,
        "pause_agent",
        json!({"agentId": "gtui/polecats/ghost"}),
    )
    .expect("pause_agent always Ok, even when gt subprocess fails");
    assert_eq!(value["kind"], "pause-agent");
    assert_eq!(value["target"], "gtui/polecats/ghost");
    assert!(
        value["command"].as_str().unwrap_or("").contains("gt nudge"),
        "pause routes through gt nudge: {}",
        value["command"]
    );
}

#[test]
fn inject_message_rejects_blank_over_ipc() {
    let store = SnapshotStore::new(isolated_root());
    let app = build_app(store);
    let webview = new_webview(&app);

    let err = invoke_json(
        &webview,
        "inject_message",
        json!({"agentId": "gtui/polecats/nux", "message": "   "}),
    )
    .expect_err("blank messages must error");
    assert!(err.as_str().unwrap_or("").contains("empty"), "got: {err}");
}

#[test]
fn write_terminal_validates_target_and_text_over_ipc() {
    let store = SnapshotStore::new(isolated_root());
    // A known-but-session-less agent so the handler reaches the has_session
    // branch rather than the unknown-target branch for the non-blank case.
    let snap = WorkspaceSnapshot {
        agents: vec![AgentInfo {
            target: "gtui/polecats/dormant".into(),
            has_session: false,
            ..AgentInfo::default()
        }],
        ..WorkspaceSnapshot::default()
    };
    store.install_snapshot(snap);
    let app = build_app(store);
    let webview = new_webview(&app);

    let blank = invoke_json(
        &webview,
        "write_terminal",
        json!({"agentId": "gtui/polecats/dormant", "text": ""}),
    )
    .expect_err("blank text must error");
    assert!(
        blank.as_str().unwrap_or("").contains("empty"),
        "got: {blank}"
    );

    let unknown = invoke_json(
        &webview,
        "write_terminal",
        json!({"agentId": "gtui/polecats/ghost", "text": "hello"}),
    )
    .expect_err("unknown target must error");
    assert!(
        unknown
            .as_str()
            .unwrap_or("")
            .contains("Unknown terminal target"),
        "got: {unknown}"
    );

    let dormant = invoke_json(
        &webview,
        "write_terminal",
        json!({"agentId": "gtui/polecats/dormant", "text": "hello"}),
    )
    .expect_err("session-less target must error");
    assert!(
        dormant
            .as_str()
            .unwrap_or("")
            .contains("does not currently have"),
        "got: {dormant}"
    );
}
