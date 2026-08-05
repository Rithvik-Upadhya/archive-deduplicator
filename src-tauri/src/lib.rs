mod commands;
mod consolidate;
mod db;
mod dbio;
mod dedup;
mod hashing;
mod links;
mod medium;
mod model;
mod parse;
mod pathfix;
mod rollup;
mod scan;
#[cfg(windows)]
mod scan_win;

use db::{Db, DbPath};
use std::sync::Mutex;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&dir).ok();
            let db_path = dir.join("dedup.sqlite");
            let conn = db::open(&db_path).expect("failed to open database");
            app.manage(Db(Mutex::new(conn)));
            app.manage(DbPath(db_path));
            app.manage(hashing::HashCancelFlags::default());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::workspace_list,
            commands::workspace_create,
            commands::workspace_rename,
            commands::workspace_delete,
            commands::source_list,
            commands::import_tree_json,
            commands::scan_folder,
            commands::detect_medium,
            commands::preview_scan,
            commands::get_size_buckets,
            commands::preview_hash_threshold,
            commands::set_hash_min_size,
            commands::hash_settings_get,
            commands::hash_settings_set,
            commands::run_hash_scan,
            commands::cancel_hash_scan,
            commands::get_scan_progress,
            commands::source_rename_device,
            commands::source_set_excluded,
            commands::source_copy_to_workspace,
            commands::source_delete,
            commands::get_tree,
            commands::run_dedup,
            commands::get_groups,
            commands::get_group_for_node,
            commands::consolidation_get,
            commands::consolidation_add_node,
            commands::consolidation_add_source_subtree,
            commands::consolidation_move_node,
            commands::consolidation_rename_node,
            commands::consolidation_delete_node,
            commands::pathfix_tree,
            commands::pathfix_rename,
            commands::app_state_get,
            commands::app_state_set,
            commands::workspace_state_get,
            commands::workspace_state_set,
            commands::db_export,
            commands::db_import,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
