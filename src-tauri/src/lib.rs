pub mod command;
pub mod config;
pub mod ipc;
pub mod models;
pub mod parse;
pub mod sessions;
pub mod snapshot;

use crate::config::{default_gt_root, env_flag, install_default_tool_path};
use crate::snapshot::SnapshotStore;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_default_tool_path();
    let open_devtools = env_flag("GTUI_OPEN_DEVTOOLS");
    let store = SnapshotStore::new(default_gt_root());
    let poller = store.clone();
    tauri::Builder::default()
        .setup(move |app| {
            // `SnapshotStore::spawn` calls `tokio::spawn` internally, which
            // requires a current runtime. Tauri's setup hook runs on the
            // main thread outside any runtime, so schedule the spawn on
            // Tauri's managed async runtime instead — the outer task
            // enters the tokio context before the inner spawn runs.
            tauri::async_runtime::spawn(async move {
                poller.spawn();
            });
            #[cfg(debug_assertions)]
            if open_devtools {
                if let Some(window) = app.get_webview_window("main") {
                    window.open_devtools();
                }
            }
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
