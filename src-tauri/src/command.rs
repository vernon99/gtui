//! Subprocess execution for `gt` / `bd` / `git` CLI calls.
//!
//! Port of `run_command` / `CommandResult` from `webui/server.py` (≈ lines
//! 390–493). Semantics preserved: success => `ok = true` with parsed or raw
//! stdout in `data`; timeout/non-zero exit/JSON failure => `ok = false` with a
//! populated `error` field. The Python `to_error()` helper is kept as a method
//! so callers that splat errors into the snapshot surface can use it verbatim.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::config::DEFAULT_COMMAND_TIMEOUT;

/// Structured result of one subprocess invocation.
///
/// Mirrors the Python `CommandResult` dataclass. `data` is `null` for failed
/// runs, a JSON value when `parse_json = true` succeeded, or a JSON string
/// (raw stdout) otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandResult {
    pub ok: bool,
    pub args: Vec<String>,
    pub cwd: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub returncode: Option<i32>,
}

impl CommandResult {
    /// Render a serializable error payload suitable for the snapshot's
    /// `errors` list. Matches `CommandResult.to_error()` in the Python port.
    pub fn to_error(&self) -> Value {
        let message = if !self.error.is_empty() {
            self.error.clone()
        } else if !self.stderr.is_empty() {
            self.stderr.clone()
        } else if !self.stdout.is_empty() {
            self.stdout.clone()
        } else {
            "command failed".to_string()
        };
        serde_json::json!({
            "command": display_command(&self.args),
            "cwd": self.cwd,
            "duration_ms": self.duration_ms,
            "error": message,
            "returncode": self.returncode,
        })
    }
}

/// Options accepted by [`run_command`]. Defaults mirror the Python signature:
/// 3s timeout, no JSON parsing, no stdin.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub timeout: Option<Duration>,
    pub parse_json: bool,
    pub stdin_text: Option<String>,
}

impl RunOptions {
    pub fn with_timeout(mut self, dur: Duration) -> Self {
        self.timeout = Some(dur);
        self
    }

    pub fn parse_json(mut self) -> Self {
        self.parse_json = true;
        self
    }

    pub fn with_stdin(mut self, stdin: impl Into<String>) -> Self {
        self.stdin_text = Some(stdin.into());
        self
    }
}

/// Shell out to `args[0] args[1..]` with `cwd` and the given options.
///
/// Never panics on non-zero exit; instead returns a `CommandResult` with
/// `ok = false`. Honours the requested timeout via `tokio::time::timeout`.
pub async fn run_command<S>(args: &[S], cwd: &Path, options: RunOptions) -> CommandResult
where
    S: AsRef<str>,
{
    let argv: Vec<String> = args.iter().map(|a| a.as_ref().to_string()).collect();
    let cwd_str = path_to_string(cwd);
    let timeout_dur = options.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT);
    let started = Instant::now();

    if argv.is_empty() {
        return CommandResult {
            ok: false,
            args: argv,
            cwd: cwd_str,
            duration_ms: 0,
            data: None,
            stdout: String::new(),
            stderr: String::new(),
            error: "empty command".to_string(),
            returncode: None,
        };
    }

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(if options.stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return CommandResult {
                ok: false,
                args: argv,
                cwd: cwd_str,
                duration_ms: elapsed_ms(started),
                data: None,
                stdout: String::new(),
                stderr: String::new(),
                error: err.to_string(),
                returncode: None,
            };
        }
    };

    if let Some(stdin_text) = options.stdin_text.as_deref() {
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(err) = stdin.write_all(stdin_text.as_bytes()).await {
                return CommandResult {
                    ok: false,
                    args: argv,
                    cwd: cwd_str,
                    duration_ms: elapsed_ms(started),
                    data: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: format!("failed to write stdin: {err}"),
                    returncode: None,
                };
            }
            drop(stdin);
        }
    }

    let wait_with_output = child.wait_with_output();
    let output = match timeout(timeout_dur, wait_with_output).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return CommandResult {
                ok: false,
                args: argv,
                cwd: cwd_str,
                duration_ms: elapsed_ms(started),
                data: None,
                stdout: String::new(),
                stderr: String::new(),
                error: err.to_string(),
                returncode: None,
            };
        }
        Err(_) => {
            return CommandResult {
                ok: false,
                args: argv,
                cwd: cwd_str,
                duration_ms: elapsed_ms(started),
                data: None,
                stdout: String::new(),
                stderr: String::new(),
                error: format!("timed out after {:.1}s", timeout_dur.as_secs_f32()),
                returncode: None,
            };
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let returncode = output.status.code();
    let duration_ms = elapsed_ms(started);

    if !output.status.success() {
        let error = if !stderr.is_empty() {
            stderr.clone()
        } else if !stdout.is_empty() {
            stdout.clone()
        } else {
            match returncode {
                Some(code) => format!("exit {code}"),
                None => "process terminated by signal".to_string(),
            }
        };
        return CommandResult {
            ok: false,
            args: argv,
            cwd: cwd_str,
            duration_ms,
            data: None,
            stdout,
            stderr,
            error,
            returncode,
        };
    }

    if options.parse_json {
        let payload = if stdout.is_empty() {
            "null"
        } else {
            stdout.as_str()
        };
        match serde_json::from_str::<Value>(payload) {
            Ok(value) => CommandResult {
                ok: true,
                args: argv,
                cwd: cwd_str,
                duration_ms,
                data: Some(value),
                stdout: String::new(),
                stderr: String::new(),
                error: String::new(),
                returncode,
            },
            Err(err) => CommandResult {
                ok: false,
                args: argv,
                cwd: cwd_str,
                duration_ms,
                data: None,
                stdout,
                stderr,
                error: format!("invalid JSON: {err}"),
                returncode,
            },
        }
    } else {
        CommandResult {
            ok: true,
            args: argv,
            cwd: cwd_str,
            duration_ms,
            data: Some(Value::String(stdout)),
            stdout: String::new(),
            stderr: String::new(),
            error: String::new(),
            returncode,
        }
    }
}

fn display_command(args: &[String]) -> String {
    args.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | '=' | ':' | ',')
        })
    {
        s.to_string()
    } else {
        let escaped = s.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Convenience constructor for call sites that want to record a synthetic
/// failure against a path they never actually invoked (e.g. a store whose
/// `.beads` directory is missing).
pub fn synthetic_error(args: Vec<String>, cwd: &Path, error: impl Into<String>) -> CommandResult {
    CommandResult {
        ok: false,
        args,
        cwd: path_to_string(cwd),
        duration_ms: 0,
        data: None,
        stdout: String::new(),
        stderr: String::new(),
        error: error.into(),
        returncode: None,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Returns a CWD suitable for tests that don't care about the working dir.
#[cfg(test)]
pub(crate) fn test_cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn success_returns_stdout_as_json_string() {
        let result = run_command(
            &["sh", "-c", "printf 'hello\\n'"],
            &test_cwd(),
            RunOptions::default(),
        )
        .await;

        assert!(result.ok, "expected success, got {result:?}");
        assert_eq!(result.returncode, Some(0));
        assert_eq!(result.data, Some(Value::String("hello".to_string())));
        assert!(result.error.is_empty());
    }

    #[tokio::test]
    async fn failure_populates_error_and_stderr() {
        let result = run_command(
            &["sh", "-c", "echo boom >&2; exit 7"],
            &test_cwd(),
            RunOptions::default(),
        )
        .await;

        assert!(!result.ok);
        assert_eq!(result.returncode, Some(7));
        assert_eq!(result.stderr, "boom");
        assert_eq!(result.error, "boom");
        assert!(result.data.is_none());
    }

    #[tokio::test]
    async fn parse_json_returns_parsed_value() {
        let result = run_command(
            &["sh", "-c", "printf '{\"rigs\":{\"gtui\":{}}}'"],
            &test_cwd(),
            RunOptions::default().parse_json(),
        )
        .await;

        assert!(result.ok, "expected json parse success, got {result:?}");
        assert_eq!(result.data, Some(json!({"rigs": {"gtui": {}}})));
    }

    #[tokio::test]
    async fn parse_json_flags_invalid_payload() {
        let result = run_command(
            &["sh", "-c", "printf 'not-json'"],
            &test_cwd(),
            RunOptions::default().parse_json(),
        )
        .await;

        assert!(!result.ok);
        assert!(
            result.error.starts_with("invalid JSON:"),
            "error was {:?}",
            result.error
        );
        assert_eq!(result.stdout, "not-json");
        assert_eq!(result.returncode, Some(0));
    }

    #[tokio::test]
    async fn timeout_is_reported_as_timed_out() {
        let result = run_command(
            &["sh", "-c", "sleep 2"],
            &test_cwd(),
            RunOptions::default().with_timeout(Duration::from_millis(150)),
        )
        .await;

        assert!(!result.ok);
        assert!(
            result.error.starts_with("timed out after"),
            "error was {:?}",
            result.error
        );
        assert!(result.returncode.is_none());
    }

    #[tokio::test]
    async fn missing_binary_returns_spawn_error() {
        let result = run_command(
            &["definitely-not-a-real-binary-xyz"],
            &test_cwd(),
            RunOptions::default(),
        )
        .await;

        assert!(!result.ok);
        assert!(!result.error.is_empty());
        assert!(result.returncode.is_none());
    }

    #[tokio::test]
    async fn stdin_is_piped_to_child() {
        let result = run_command(
            &["sh", "-c", "cat"],
            &test_cwd(),
            RunOptions::default().with_stdin("payload-from-parent"),
        )
        .await;

        assert!(result.ok, "expected success, got {result:?}");
        assert_eq!(
            result.data,
            Some(Value::String("payload-from-parent".to_string()))
        );
    }

    #[test]
    fn to_error_prefers_error_then_stderr_then_stdout() {
        let mut r = CommandResult {
            ok: false,
            args: vec!["gt".into(), "status".into()],
            cwd: "/tmp".into(),
            duration_ms: 12,
            data: None,
            stdout: "out".into(),
            stderr: "err".into(),
            error: "explicit".into(),
            returncode: Some(1),
        };
        assert_eq!(r.to_error()["error"], "explicit");
        r.error.clear();
        assert_eq!(r.to_error()["error"], "err");
        r.stderr.clear();
        assert_eq!(r.to_error()["error"], "out");
        r.stdout.clear();
        assert_eq!(r.to_error()["error"], "command failed");
    }

    #[test]
    fn to_error_renders_command_with_quoting() {
        let r = CommandResult {
            ok: false,
            args: vec!["gt".into(), "feed".into(), "--since".into(), "20 m".into()],
            cwd: "/tmp".into(),
            duration_ms: 0,
            data: None,
            stdout: String::new(),
            stderr: String::new(),
            error: "nope".into(),
            returncode: None,
        };
        assert_eq!(r.to_error()["command"], "gt feed --since '20 m'");
    }

    #[test]
    fn command_result_round_trips_through_json() {
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
}
