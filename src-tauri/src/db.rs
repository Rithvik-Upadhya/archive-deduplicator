//! SQLite persistence layer. A single connection is guarded by a mutex and
//! shared through Tauri's managed state. The schema is created on first run.

use rusqlite::Connection;
use std::sync::Mutex;

/// Managed database handle stored in Tauri state.
pub struct Db(pub Mutex<Connection>);

impl Db {
    /// Lock the connection, recovering from poisoning instead of panicking.
    ///
    /// A panic anywhere in a command while holding this lock (an indexing
    /// bug, an unexpected-data `unwrap()`) would otherwise poison the mutex
    /// permanently, bricking every subsequent command for the rest of the
    /// app session. SQLite's transactional model makes recovery safe here --
    /// a panic mid-transaction just leaves it rolled back to the last
    /// commit, so the recovered connection is never left half-written.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The on-disk path of the managed database, stored alongside `Db` so a
/// command can open its own dedicated connection (e.g. `run_dedup`, which
/// must not hold `Db`'s mutex for the duration of a long-running pass).
pub struct DbPath(pub std::path::PathBuf);

/// Open (creating if needed) the database at `path` and ensure the schema exists.
pub fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    init_schema(&conn)?;
    migrate(&conn)?;
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
            file_count INTEGER NOT NULL DEFAULT 0,
            excluded INTEGER NOT NULL DEFAULT 0,
            physical_size INTEGER NOT NULL DEFAULT 0,
            alias_bytes INTEGER NOT NULL DEFAULT 0,
            medium_kind TEXT,
            filesystem TEXT,
            hash_min_size INTEGER NOT NULL DEFAULT 65536,
            hash_spec TEXT,
            hash_coverage_files INTEGER NOT NULL DEFAULT 0,
            hash_coverage_bytes INTEGER NOT NULL DEFAULT 0,
            volume_id TEXT,
            hashing_enabled INTEGER NOT NULL DEFAULT 1
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
            subtree_file_count INTEGER NOT NULL DEFAULT 0,
            alias_of INTEGER REFERENCES nodes(id) ON DELETE SET NULL,
            inode_trusted INTEGER NOT NULL DEFAULT 0,
            inode_high INTEGER,
            content_hash BLOB,
            hash_kind TEXT,
            hash_spec TEXT,
            hash_bytes_read INTEGER,
            hashed_at TEXT,
            listing_hash BLOB,
            link_target TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_nodes_source ON nodes(source_id);
        CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes(parent_id);
        CREATE INDEX IF NOT EXISTS idx_nodes_size ON nodes(size);
        CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(type);
        -- get_tree's hot lazy-load path filters on both columns together
        -- (WHERE source_id = ? AND parent_id = ?) on every tree-expand
        -- click; the single-column indexes above only let SQLite use one
        -- and scan-filter the other.
        CREATE INDEX IF NOT EXISTS idx_nodes_source_parent ON nodes(source_id, parent_id);
        -- idx_nodes_alias/idx_nodes_inode/idx_nodes_hash reference columns
        -- (alias_of / inode_trusted / content_hash) that only exist here
        -- because this same statement just created `nodes` from scratch.
        -- On an existing pre-migration database, `CREATE TABLE IF NOT
        -- EXISTS` above is a no-op against the old-shaped table, so those
        -- columns wouldn't exist yet -- creating the indexes here too would
        -- crash *before* `migrate()` ever runs to add them. `migrate` creates
        -- these same indexes itself, safely, after its ALTER TABLE loop.

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
            sort_order INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_cnodes_consolidation ON consolidation_nodes(consolidation_id);
        CREATE INDEX IF NOT EXISTS idx_cnodes_parent ON consolidation_nodes(parent_id);

        -- Legacy source-based path edits table (superseded by pathfix_state).
        DROP TABLE IF EXISTS pathfix_edits;

        -- Legacy audit trail / guide-export feature, removed.
        DROP TABLE IF EXISTS action_log;

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

        -- Per-workspace settings (dedup tuning, staleness), unlike app_state
        -- above which is app-wide (active workspace, current view).
        CREATE TABLE IF NOT EXISTS workspace_state (
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (workspace_id, key)
        );

        -- Precomputed duplicate annotations per node, rebuilt after each dedup
        -- run so tree browsing never has to recompute them on the fly.
        CREATE TABLE IF NOT EXISTS dup_annot (
            node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
            has_dup INTEGER NOT NULL DEFAULT 0,
            dup_pct REAL NOT NULL DEFAULT 0,
            cross_dup INTEGER NOT NULL DEFAULT 0,
            cross_dup_size INTEGER NOT NULL DEFAULT 0,
            cross_dup_file_count INTEGER NOT NULL DEFAULT 0,
            in_folder_group INTEGER NOT NULL DEFAULT 0
        );

        -- Survives source deletion and re-import so a re-scan of the same
        -- medium is nearly free: a cache hit requires size, mtime, AND spec
        -- to all match, so any edit or spec change invalidates it.
        CREATE TABLE IF NOT EXISTS hash_cache (
            volume_id TEXT NOT NULL,
            file_id INTEGER NOT NULL,
            size INTEGER NOT NULL,
            mtime TEXT NOT NULL,
            hash_spec TEXT NOT NULL,
            content_hash BLOB NOT NULL,
            hash_kind TEXT NOT NULL,
            computed_at TEXT NOT NULL,
            PRIMARY KEY (volume_id, file_id, size, mtime, hash_spec)
        );

        -- Resumability for a multi-hour hashing pass. `last_cursor` is
        -- INFORMATIONAL ONLY: the cumulative count of files hashed for this
        -- source, for progress display. It is NOT an index into a candidate
        -- list and must never be used to slice one -- resumption works purely
        -- off `content_hash IS NULL` (see hashing.rs::run_hash_scan).
        CREATE TABLE IF NOT EXISTS scan_progress (
            source_id INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
            phase TEXT NOT NULL,
            last_cursor INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// Target schema version. Bump this and add an entry to `migrate`'s
/// `alterations` list whenever a column is added to an already-shipped table.
pub const SCHEMA_VERSION: i64 = 7;

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1");
    let count: i64 = conn.query_row(&sql, rusqlite::params![column], |r| r.get(0))?;
    Ok(count > 0)
}

/// Bring an existing on-disk database up to `SCHEMA_VERSION`. `init_schema`'s
/// `CREATE TABLE IF NOT EXISTS` only ever helps a brand-new database -- on an
/// existing one it silently no-ops against the already-present old-shape
/// table, so newly added columns need an explicit `ALTER TABLE` here.
///
/// Checked via `column_exists` (introspection) rather than purely by
/// `user_version`, so this stays correct regardless of whether a given
/// column already exists (e.g. on a database `init_schema` just created
/// fresh, where every column in the list below is already present).
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // `PRAGMA user_version` is a pure read of the database header -- checking
    // it first keeps the common (already-migrated) case a read-only no-op,
    // same as `init_schema`'s `CREATE TABLE IF NOT EXISTS` statements. Every
    // `db::open()` call runs this, including dedicated connections opened
    // alongside an in-progress writer (see `run_dedup`'s doc comment and the
    // `wal_mode_lets_a_reader_proceed_during_an_open_writer_transaction`
    // test below) -- an unconditional write here would reintroduce exactly
    // the lock contention that design depends on avoiding.
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    let alterations: &[(&str, &str, &str)] = &[
        (
            "nodes",
            "alias_of",
            "ALTER TABLE nodes ADD COLUMN alias_of INTEGER REFERENCES nodes(id) ON DELETE SET NULL",
        ),
        (
            "nodes",
            "inode_trusted",
            "ALTER TABLE nodes ADD COLUMN inode_trusted INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "sources",
            "physical_size",
            "ALTER TABLE sources ADD COLUMN physical_size INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "sources",
            "alias_bytes",
            "ALTER TABLE sources ADD COLUMN alias_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "nodes",
            "inode_high",
            "ALTER TABLE nodes ADD COLUMN inode_high INTEGER",
        ),
        (
            "nodes",
            "content_hash",
            "ALTER TABLE nodes ADD COLUMN content_hash BLOB",
        ),
        (
            "nodes",
            "hash_kind",
            "ALTER TABLE nodes ADD COLUMN hash_kind TEXT",
        ),
        (
            "nodes",
            "hash_spec",
            "ALTER TABLE nodes ADD COLUMN hash_spec TEXT",
        ),
        (
            "nodes",
            "hash_bytes_read",
            "ALTER TABLE nodes ADD COLUMN hash_bytes_read INTEGER",
        ),
        (
            "nodes",
            "hashed_at",
            "ALTER TABLE nodes ADD COLUMN hashed_at TEXT",
        ),
        (
            "nodes",
            "listing_hash",
            "ALTER TABLE nodes ADD COLUMN listing_hash BLOB",
        ),
        (
            "sources",
            "medium_kind",
            "ALTER TABLE sources ADD COLUMN medium_kind TEXT",
        ),
        (
            "sources",
            "filesystem",
            "ALTER TABLE sources ADD COLUMN filesystem TEXT",
        ),
        (
            "sources",
            "hash_min_size",
            "ALTER TABLE sources ADD COLUMN hash_min_size INTEGER NOT NULL DEFAULT 65536",
        ),
        (
            "sources",
            "hash_spec",
            "ALTER TABLE sources ADD COLUMN hash_spec TEXT",
        ),
        (
            "sources",
            "hash_coverage_files",
            "ALTER TABLE sources ADD COLUMN hash_coverage_files INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "sources",
            "hash_coverage_bytes",
            "ALTER TABLE sources ADD COLUMN hash_coverage_bytes INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "nodes",
            "link_target",
            "ALTER TABLE nodes ADD COLUMN link_target TEXT",
        ),
        (
            "sources",
            "volume_id",
            "ALTER TABLE sources ADD COLUMN volume_id TEXT",
        ),
        (
            "sources",
            "hashing_enabled",
            "ALTER TABLE sources ADD COLUMN hashing_enabled INTEGER NOT NULL DEFAULT 1",
        ),
    ];
    for (table, column, ddl) in alterations {
        if !column_exists(conn, table, column)? {
            conn.execute(ddl, [])?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_nodes_alias ON nodes(alias_of) WHERE alias_of IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_nodes_inode ON nodes(source_id, dev, inode) WHERE inode IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_nodes_hash ON nodes(content_hash) WHERE content_hash IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_nodes_source_parent ON nodes(source_id, parent_id);
         CREATE TABLE IF NOT EXISTS hash_cache (
             volume_id TEXT NOT NULL, file_id INTEGER NOT NULL, size INTEGER NOT NULL,
             mtime TEXT NOT NULL, hash_spec TEXT NOT NULL, content_hash BLOB NOT NULL,
             hash_kind TEXT NOT NULL, computed_at TEXT NOT NULL,
             PRIMARY KEY (volume_id, file_id, size, mtime, hash_spec)
         );
         -- Resumability for a multi-hour hashing pass. `last_cursor` is
         -- INFORMATIONAL ONLY: the cumulative count of files hashed for this
         -- source, for progress display. It is NOT an index into a candidate
         -- list and must never be used to slice one -- resumption works purely
         -- off `content_hash IS NULL` (see hashing.rs::run_hash_scan).
         CREATE TABLE IF NOT EXISTS scan_progress (
             source_id INTEGER PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
             phase TEXT NOT NULL, last_cursor INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL
         );",
    )?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `run_dedup` opens its own connection instead of locking the shared
    /// `Db` mutex, so other commands stay responsive while it runs. That only
    /// works if WAL mode genuinely lets a reader proceed while a second
    /// connection holds an open write transaction on the same file -- this
    /// test is the empirical proof, not just a comment.
    #[test]
    fn wal_mode_lets_a_reader_proceed_during_an_open_writer_transaction() {
        let path = std::env::temp_dir().join(format!(
            "archive-dedup-wal-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _cleanup = CleanupOnDrop(path.clone());

        let writer = open(&path).unwrap();
        writer
            .execute(
                "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
                [],
            )
            .unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer
            .execute(
                "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w2', 't', 't')",
                [],
            )
            .unwrap();
        // The write transaction above is deliberately left open (no COMMIT).

        let reader = open(&path).unwrap();
        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM workspaces", [], |r| r.get(0))
            .expect("reader must not block/fail while a writer transaction is open under WAL");
        assert_eq!(
            count, 1,
            "reader sees only the committed row, not the open transaction's"
        );

        writer.execute_batch("COMMIT").unwrap();
    }

    struct CleanupOnDrop(std::path::PathBuf);
    impl Drop for CleanupOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-wal"));
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-shm"));
        }
    }

    /// A stand-in for a pre-migration on-disk database: the baseline tables
    /// `migrate` needs to alter, deliberately missing every column added
    /// since (hardlink columns, then medium/hash/listing columns).
    const OLD_SCHEMA_DDL: &str = r#"
        CREATE TABLE workspaces (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE sources (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_id INTEGER NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            label TEXT NOT NULL,
            device_label TEXT NOT NULL,
            orig_root_path TEXT,
            dev_id INTEGER,
            imported_at TEXT NOT NULL,
            total_size INTEGER NOT NULL DEFAULT 0,
            file_count INTEGER NOT NULL DEFAULT 0,
            excluded INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE nodes (
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
    "#;

    #[test]
    fn migrate_adds_new_columns_to_a_pre_migration_database() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_SCHEMA_DDL).unwrap();

        migrate(&conn).unwrap();

        assert!(column_exists(&conn, "nodes", "alias_of").unwrap());
        assert!(column_exists(&conn, "nodes", "inode_trusted").unwrap());
        assert!(column_exists(&conn, "sources", "physical_size").unwrap());
        assert!(column_exists(&conn, "sources", "alias_bytes").unwrap());
        for col in [
            "inode_high",
            "content_hash",
            "hash_kind",
            "hash_spec",
            "hash_bytes_read",
            "hashed_at",
            "listing_hash",
            "link_target",
        ] {
            assert!(column_exists(&conn, "nodes", col).unwrap(), "nodes.{col}");
        }
        for col in [
            "medium_kind",
            "filesystem",
            "hash_min_size",
            "hash_spec",
            "hash_coverage_files",
            "hash_coverage_bytes",
            "volume_id",
            "hashing_enabled",
        ] {
            assert!(
                column_exists(&conn, "sources", col).unwrap(),
                "sources.{col}"
            );
        }
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn migrate_creates_hash_cache_and_scan_progress_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(OLD_SCHEMA_DDL).unwrap();

        migrate(&conn).unwrap();

        for table in ["hash_cache", "scan_progress"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "{table} should exist after migrate");
        }
    }

    #[test]
    fn migrate_is_idempotent_on_a_fresh_database() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second call must not error (no double ALTER)
    }

    /// Regression test for a real startup crash: `open()` (unlike the tests
    /// above) runs `init_schema` and `migrate` back-to-back against the
    /// *same* on-disk, already-populated pre-migration database -- exactly
    /// what happens on a user's machine. `init_schema`'s `CREATE TABLE IF NOT
    /// EXISTS nodes` no-ops against the existing old-shaped table, so if
    /// `init_schema` also tried to create an index on a column that only
    /// `migrate` adds (as it once did for `idx_nodes_hash` on `content_hash`),
    /// it would crash before `migrate` ever got to run.
    #[test]
    fn open_succeeds_against_an_existing_pre_migration_database_file() {
        let path = std::env::temp_dir().join(format!(
            "archive-dedup-premigration-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _cleanup = CleanupOnDrop(path.clone());

        {
            let seed = Connection::open(&path).unwrap();
            seed.execute_batch(OLD_SCHEMA_DDL).unwrap();
        }

        // Must not panic/error -- this is the exact call `lib.rs`'s `setup`
        // makes on every app launch.
        let conn = open(&path).unwrap();

        for col in ["alias_of", "inode_trusted", "content_hash", "listing_hash"] {
            assert!(column_exists(&conn, "nodes", col).unwrap(), "nodes.{col}");
        }
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
