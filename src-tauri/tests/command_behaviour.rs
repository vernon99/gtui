//! Integration tests for the subprocess execution layer.
//!
//! These exercise `run_command` through the public API (rather than through
//! the module's private test helpers) so we also catch breakage in how
//! `CommandResult` is exposed for downstream consumers like the snapshot
//! store and IPC handlers.

use std::time::Duration;

use gtui_lib::command::{display_command, run_command, synthetic_error, CommandResult, RunOptions};
use serde_json::{json, Value};

fn cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[tokio::test]
async fn run_command_success_preserves_stdout_as_data() {
    let result = run_command(
        &["sh", "-c", "printf 'integration-ok'"],
        &cwd(),
        RunOptions::default(),
    )
    .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(
        result.data,
        Some(Value::String("integration-ok".to_string()))
    );
    assert_eq!(result.returncode, Some(0));
}

#[tokio::test]
async fn run_command_nonzero_exit_surfaces_stderr_as_error() {
    let result = run_command(
        &["sh", "-c", "echo 'bad news' >&2; exit 3"],
        &cwd(),
        RunOptions::default(),
    )
    .await;
    assert!(!result.ok);
    assert_eq!(result.stderr, "bad news");
    assert_eq!(result.error, "bad news");
    assert_eq!(result.returncode, Some(3));
}

#[tokio::test]
async fn run_command_signal_termination_reports_no_returncode() {
    // `kill -9 $$` from inside sh: signal-terminated children have no exit
    // code, so `returncode` must be `None` with a useful error message.
    let result = run_command(
        &["sh", "-c", "kill -9 $$"],
        &cwd(),
        RunOptions::default().with_timeout(Duration::from_secs(2)),
    )
    .await;
    assert!(!result.ok);
    assert!(result.returncode.is_none(), "got {:?}", result.returncode);
    assert!(!result.error.is_empty());
}

#[tokio::test]
async fn run_command_parse_json_empty_stdout_deserialises_as_null() {
    // the contract's behaviour: empty stdout with parse_json becomes `null`, not an
    // error. Verify the Rust port matches.
    let result = run_command(
        &["sh", "-c", "true"],
        &cwd(),
        RunOptions::default().parse_json(),
    )
    .await;
    assert!(result.ok, "{result:?}");
    assert_eq!(result.data, Some(Value::Null));
}

#[tokio::test]
async fn run_command_timeout_error_message_mentions_timeout() {
    let result = run_command(
        &["sh", "-c", "sleep 5"],
        &cwd(),
        RunOptions::default().with_timeout(Duration::from_millis(120)),
    )
    .await;
    assert!(!result.ok);
    assert!(
        result.error.starts_with("timed out after"),
        "unexpected error: {}",
        result.error
    );
    assert!(result.returncode.is_none());
}

#[tokio::test]
async fn run_command_stdin_round_trips_through_cat() {
    let result = run_command(
        &["sh", "-c", "cat"],
        &cwd(),
        RunOptions::default().with_stdin("piped-input\nwith newline"),
    )
    .await;
    assert!(result.ok, "{result:?}");
    let stdout = result.data.as_ref().and_then(Value::as_str).unwrap_or("");
    assert!(stdout.contains("piped-input"));
}

#[tokio::test]
async fn run_command_empty_argv_fails_fast() {
    let empty: [&str; 0] = [];
    let result = run_command(&empty, &cwd(), RunOptions::default()).await;
    assert!(!result.ok);
    assert_eq!(result.error, "empty command");
    assert!(result.args.is_empty());
}

#[test]
fn synthetic_error_renders_without_running_subprocess() {
    let err = synthetic_error(
        vec!["gt".into(), "status".into()],
        &cwd(),
        "fabricated failure",
    );
    assert!(!err.ok);
    assert_eq!(err.error, "fabricated failure");
    assert_eq!(err.args, vec!["gt", "status"]);
    assert_eq!(err.duration_ms, 0);
}

#[test]
fn command_result_to_error_quotes_whitespace_args() {
    let r = CommandResult {
        ok: false,
        args: vec!["gt".into(), "feed".into(), "--since".into(), "20 m".into()],
        cwd: cwd().to_string_lossy().into_owned(),
        duration_ms: 7,
        data: None,
        stdout: String::new(),
        stderr: String::new(),
        error: "nope".into(),
        returncode: None,
    };
    let rendered = r.to_error();
    assert_eq!(rendered["command"], "gt feed --since '20 m'");
    assert_eq!(rendered["error"], "nope");
    assert_eq!(rendered["duration_ms"], 7);
}

#[test]
fn display_command_leaves_safe_args_unquoted() {
    let args = vec!["gt", "status", "--fast"];
    assert_eq!(display_command(&args), "gt status --fast");
}

#[test]
fn command_result_json_round_trip_preserves_every_field() {
    let original = CommandResult {
        ok: true,
        args: vec!["gt".into(), "status".into()],
        cwd: "/workspace".into(),
        duration_ms: 42,
        data: Some(json!({"town": "gastown"})),
        stdout: String::new(),
        stderr: String::new(),
        error: String::new(),
        returncode: Some(0),
    };
    let encoded = serde_json::to_string(&original).expect("encode");
    let decoded: CommandResult = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(original, decoded);
}
