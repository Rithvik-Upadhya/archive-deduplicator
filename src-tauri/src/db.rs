//! SQLite persistence layer. A single connection is guarded by a mutex and
//! shared through Tauri's managed state. The schema is created on first run.

use rusqlite::Connection;
use std::sync::Mutex;

/// Managed database handle stored in Tauri state.
pub struct Db(pub Mutex<Connection>);

/// Open (creating if needed) the database at `path` and ensure the schema exists.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Create all tables and indexes if they do not already exist.
pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS workspaces (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            label TEXT NOT NULL,
            device_label TEXT NOT NULL,
            orig_root_path TEXT,
            dev_id INTEGER,
            imported_at TEXT NOT NULL,
            total_size INTEGER NOT NULL DEFAULT 0,
            file_count INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS nodes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
            parent_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            rel_path TEXT NOT NULL,
            type TEXT NOT NULL,
            size INTEGER NOT NULL DEFAULT 0,
            mtime TEXT,
            inode INTEGER,
            dev INTEGER,
            depth INTEGER NOT NULL DEFAULT 0,
            subtree_size INTEGER NOT NULL DEFAULT 0,
            subtree_file_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_source ON nodes(source_id);
        CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent_id);
        CREATE INDEX IF NOT EXISTS idx_nodes_size ON nodes(size);
        CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(type);

        CREATE TABLE IF NOT EXISTS match_groups (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0,
            primary_signal TEXT NOT NULL DEFAULT '',
            size INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_groups_ws ON match_groups(workspace_id);

        CREATE TABLE IF NOT EXISTS match_members (
            group_id INTEGER NOT NULL REFERENCES match_groups(id) ON DELETE CASCADE,
            node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            role TEXT NOT NULL DEFAULT 'member',
            PRIMARY KEY (group_id, node_id)
        );
        CREATE INDEX IF NOT EXISTS idx_members_node ON match_members(node_id);
        CREATE INDEX IF NOT EXISTS idx_members_group ON match_members(group_id);

        CREATE TABLE IF NOT EXISTS consolidations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            name TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS consolidation_nodes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            consolidation_id INTEGER NOT NULL REFERENCES consolidations(id) ON DELETE CASCADE,
            parent_id INTEGER REFERENCES consolidation_nodes(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            type TEXT NOT NULL,
            source_node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            action TEXT NOT NULL DEFAULT 'keep',
            sort_order INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_cnodes_consolidation ON consolidation_nodes(consolidation_id);
        CREATE INDEX IF NOT EXISTS idx_cnodes_parent ON consolidation_nodes(parent_id);

        CREATE TABLE IF NOT EXISTS action_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            ts TEXT NOT NULL,
            op TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_log_ws ON action_log(workspace_id);

        -- Legacy source-based path edits table (superseded by pathfix_state).
        DROP TABLE IF EXISTS pathfix_edits;

        -- Path-limit fixing state for the consolidated end-state tree.
        -- kind = 'cons' (ref_id -> consolidation_nodes.id) or
        --        'source' (ref_id -> nodes.id, a virtual rename of a file/folder
        --        that lives inside a dragged-in source directory).
        CREATE TABLE IF NOT EXISTS pathfix_state (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            ref_id INTEGER NOT NULL,
            new_name TEXT NOT NULL DEFAULT '',
            original_name TEXT NOT NULL DEFAULT '',
            resolved INTEGER NOT NULL DEFAULT 0,
            UNIQUE (workspace_id, kind, ref_id)
        );

        CREATE TABLE IF NOT EXISTS app_state (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        -- Precomputed duplicate annotations per node, rebuilt after each dedup
        -- run so tree browsing never has to recompute them on the fly.
        CREATE TABLE IF NOT EXISTS dup_annot (
            node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
            has_dup INTEGER NOT NULL DEFAULT 0,
            dup_pct REAL NOT NULL DEFAULT 0
        );
        "#,
    )?;
    Ok(())
}
