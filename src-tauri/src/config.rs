//! Runtime configuration constants and path resolution for GTUI.
//!
//! These constants define the snapshot cadence, cache TTLs, command timeouts,
//! and default Gas Town root used by the desktop app.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// Snapshot poll cadence.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2_000);

/// Default per-command timeout for `run_command`. Individual call sites may
/// override this (e.g. `gt polecat list --all --json` uses 6s).
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_millis(3_000);

/// `gt status --fast` can legitimately take much longer on towns with many
/// tmux sessions because it fans out through runtime inspection for each
/// session. Keep its timeout separate from the generic subprocess budget.
pub const GT_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a cached Codex rollout listing remains fresh before rescan.
pub const CODEX_ROLLOUT_LIST_TTL: Duration = Duration::from_millis(3_000);

/// How long a cached Claude session listing remains fresh before rescan.
pub const CLAUDE_SESSION_LIST_TTL: Duration = Duration::from_millis(3_000);

/// Maximum number of rollout files to scan per refresh.
pub const CODEX_ROLLOUT_SCAN_LIMIT: usize = 160;

/// Maximum number of Claude session files to scan per refresh.
pub const CLAUDE_SESSION_SCAN_LIMIT: usize = 120;

/// Maximum bytes read from the head of a transcript file for preview.
pub const CODEX_ROLLOUT_HEAD_BYTES: usize = 32_768;

/// Maximum bytes read from the head of a Claude session transcript.
pub const CLAUDE_SESSION_HEAD_BYTES: usize = 32_768;

/// Resolve the default Gas Town root: `$GT_ROOT` if set, else `$HOME/gt`, else
/// the relative path `./gt` as a last resort.
pub fn default_gt_root() -> PathBuf {
    if let Some(explicit) = std::env::var_os("GT_ROOT") {
        return PathBuf::from(explicit);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let mut root = PathBuf::from(home);
        root.push("gt");
        return root;
    }
    PathBuf::from("gt")
}

/// Ensure GUI launches can still find tools installed by the user's shell.
///
/// LaunchServices does not reliably preserve the interactive shell `PATH`, so
/// app-bundle launches may otherwise fail to find `gt`, `bd`, `tmux`, or
/// Homebrew tools even though they work in a terminal.
pub fn install_default_tool_path() {
    let additions = default_tool_path_entries(std::env::var_os("HOME"));
    let merged = merge_path_entries(std::env::var_os("PATH"), &additions);
    // SAFETY: this runs once during process startup before background worker
    // tasks are spawned.
    unsafe {
        std::env::set_var("PATH", merged);
    }
}

pub fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

fn default_tool_path_entries(home: Option<OsString>) -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if let Some(home) = home {
        let home = PathBuf::from(home);
        entries.push(home.join(".local/bin"));
        entries.push(home.join(".cargo/bin"));
    }
    entries.push(PathBuf::from("/opt/homebrew/bin"));
    entries.push(PathBuf::from("/opt/homebrew/sbin"));
    entries.push(PathBuf::from("/usr/local/bin"));
    entries
}

fn merge_path_entries(existing: Option<OsString>, additions: &[PathBuf]) -> OsString {
    let mut paths: Vec<PathBuf> = existing
        .as_ref()
        .map(|value| std::env::split_paths(value).collect())
        .unwrap_or_default();

    for entry in additions.iter().rev() {
        if !paths.iter().any(|path| path == entry) {
            paths.insert(0, entry.clone());
        }
    }

    std::env::join_paths(paths).unwrap_or_else(|_| existing.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_runtime_defaults() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(2));
        assert_eq!(DEFAULT_COMMAND_TIMEOUT, Duration::from_secs(3));
        assert_eq!(GT_STATUS_TIMEOUT, Duration::from_secs(30));
        assert_eq!(CODEX_ROLLOUT_LIST_TTL, Duration::from_secs(3));
        assert_eq!(CLAUDE_SESSION_LIST_TTL, Duration::from_secs(3));
        assert_eq!(CODEX_ROLLOUT_SCAN_LIMIT, 160);
        assert_eq!(CLAUDE_SESSION_SCAN_LIMIT, 120);
    }

    #[test]
    fn gt_root_prefers_env_override() {
        // SAFETY: tests in this module run sequentially via `--test-threads`;
        // cargo defaults are multi-threaded so guard against env races by
        // only asserting when the override is present.
        let previous = std::env::var_os("GT_ROOT");
        // SAFETY: `set_var` / `remove_var` are unsafe on recent stdlib.
        unsafe {
            std::env::set_var("GT_ROOT", "/tmp/explicit-gt-root");
        }
        assert_eq!(default_gt_root(), PathBuf::from("/tmp/explicit-gt-root"));
        unsafe {
            match previous {
                Some(value) => std::env::set_var("GT_ROOT", value),
                None => std::env::remove_var("GT_ROOT"),
            }
        }
    }

    #[test]
    fn merge_path_entries_prepends_missing_tool_dirs_without_duplicates() {
        let home = OsString::from("/Users/example");
        let additions = default_tool_path_entries(Some(home));
        let existing = OsString::from("/usr/bin:/Users/example/.local/bin:/bin");
        let merged = merge_path_entries(Some(existing), &additions);
        let paths: Vec<PathBuf> = std::env::split_paths(&merged).collect();

        assert_eq!(paths[0], PathBuf::from("/Users/example/.cargo/bin"));
        assert_eq!(paths[1], PathBuf::from("/opt/homebrew/bin"));
        assert_eq!(paths[2], PathBuf::from("/opt/homebrew/sbin"));
        assert_eq!(paths[3], PathBuf::from("/usr/local/bin"));
        assert_eq!(
            paths
                .iter()
                .filter(|path| path == &&PathBuf::from("/Users/example/.local/bin"))
                .count(),
            1
        );
    }
}
