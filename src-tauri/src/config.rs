//! Runtime configuration constants and path resolution for GTUI.
//!
//! Mirrors the top-of-file constants in `webui/server.py` (GT_ROOT, polling
//! interval, cache TTLs, default command timeout). The Rust port keeps the
//! same numeric values so polecats, witnesses, and operators observe the same
//! cadence regardless of which backend is driving the UI.

use std::path::PathBuf;
use std::time::Duration;

/// Snapshot poll cadence. The Python server refreshes once every 2 seconds.
pub const POLL_INTERVAL: Duration = Duration::from_millis(2_000);

/// Default per-command timeout for `run_command`. Individual call sites may
/// override this (e.g. `gt polecat list --all --json` uses 6s).
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_millis(3_000);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_python_defaults() {
        assert_eq!(POLL_INTERVAL, Duration::from_secs(2));
        assert_eq!(DEFAULT_COMMAND_TIMEOUT, Duration::from_secs(3));
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
}
