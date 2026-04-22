pub mod command;
pub mod config;
pub mod ipc;
pub mod models;
pub mod parse;
pub mod sessions;
pub mod snapshot;

use crate::config::default_gt_root;
use crate::snapshot::SnapshotStore;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let store = SnapshotStore::new(default_gt_root());
    let poller = store.clone();
    tauri::Builder::default()
        .setup(move |_app| {
            // Kick off the background polling task. Dropping the JoinHandle
            // does not cancel the task in tokio; cleanup happens when the
            // process exits. A future revision can wire SnapshotStore::shutdown
            // into an app-exit hook.
            std::mem::drop(poller.spawn());
            Ok(())
        })
        .manage(store)
        .invoke_handler(tauri::generate_handler![
            ipc::get_snapshot,
            ipc::get_terminal,
            ipc::get_git_diff,
            ipc::retry_task,
            ipc::pause_agent,
            ipc::inject_message,
            ipc::write_terminal,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
