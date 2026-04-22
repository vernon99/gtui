//! Build a snapshot against a given GT_ROOT and print it as JSON on stdout.
//!
//! Mirror of `GET /api/snapshot` from the legacy Python webui — used by
//! `scripts/parity_smoke.sh` to diff the two implementations side by side.
//!
//! Usage:
//!   cargo run --example snapshot_dump -- [GT_ROOT]
//! GT_ROOT defaults to $GT_ROOT, then $HOME/gt.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use gtui_lib::snapshot::build_snapshot;

fn resolve_root() -> PathBuf {
    if let Some(arg) = env::args().nth(1) {
        return PathBuf::from(arg);
    }
    if let Ok(env_root) = env::var("GT_ROOT") {
        return PathBuf::from(env_root);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("gt")
}

#[tokio::main]
async fn main() -> ExitCode {
    let root = resolve_root();
    let snapshot = build_snapshot(&root, &[]).await;
    match serde_json::to_string(&snapshot) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("snapshot_dump: failed to encode JSON: {err}");
            ExitCode::FAILURE
        }
    }
}
