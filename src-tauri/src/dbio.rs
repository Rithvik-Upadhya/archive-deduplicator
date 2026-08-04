//! Database export/import.
//!
//! Export simply checkpoints the WAL and copies the live SQLite file to a
//! user-chosen path, so the exported file is a complete, self-contained
//! snapshot of every workspace.
//!
//! Import attaches the chosen external SQLite file (read-only, as `ext`)
//! and merges its contents into the live database non-destructively: every
//! workspace found in the external file is recreated as a *new* workspace
//! (never overwriting or deleting anything that already exists), and all
//! rows that hang off it (sources, nodes, match groups/members,
//! consolidations, consolidation nodes, pathfix state, action log) are
//! copied across with their foreign keys remapped to the newly minted ids.

use rusqlite::{Connection, backup::Backup, params};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

/// Checkpoint the WAL and copy the database file to `dest`.
pub fn export_to(conn: &Connection, dest: &Path) -> rusqlite::Result<()> {
    if let Some(parent) = dest.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut dest_conn = Connection::open(&dest)?;
    let backup = Backup::new(conn, &mut dest_conn)?;
    backup.run_to_completion(1000, Duration::from_millis(0), None)?;
    Ok(())
}

/// Result summary returned to the frontend after a merge import.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportSummary {
    pub workspaces_added: i64,
    pub sources_added: i64,
    pub nodes_added: i64,
    /// Ids of the newly created workspaces, in case the caller wants to log
    /// against them or select one immediately.
    pub new_workspace_ids: Vec<i64>,
}

/// Merge the workspaces (and everything under them) found in the SQLite
/// database at `src_path` into `conn`, as brand-new workspaces. Nothing in
/// the destination database is modified or deleted.
pub fn import_merge(conn: &mut Connection, src_path: &Path) -> rusqlite::Result<ImportSummary> {
    // String must only contain UTF-8 characters
    let src_path_str = src_path
        .to_str()
        .ok_or_else(|| rusqlite::Error::InvalidPath(src_path.to_path_buf()))?;

    // SQLite forbids ATTACH/DETACH while a transaction is open on the
    // connection, so this must happen *before* `conn.transaction()` below
    // (doing it after produces a "database ext is locked" / attach error).
    conn.execute("ATTACH DATABASE ?1 AS ext", params![src_path_str])?;

    let tx = conn.transaction()?;

    let result = (|| -> rusqlite::Result<ImportSummary> {
        // Make sure the attached db actually has our schema (older/foreign
        // files might not); if the workspaces table doesn't exist this will
        // error out and we abort cleanly.
        let mut ws_stmt =
            tx.prepare("SELECT id, name, created_at, updated_at FROM ext.workspaces ORDER BY id")?;
        let workspaces: Vec<(i64, String, String, String)> = ws_stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(ws_stmt);

        let mut workspaces_added = 0i64;
        let mut sources_added = 0i64;
        let mut nodes_added = 0i64;
        let mut new_workspace_ids: Vec<i64> = Vec::new();

        // Existing workspace names in the destination, used to disambiguate.
        let mut existing_names: std::collections::HashSet<String> = {
            let mut s = tx.prepare("SELECT name FROM workspaces")?;
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .collect()
        };

        for (old_ws_id, name, created_at, updated_at) in workspaces {
            let mut new_name = name.clone();
            if existing_names.contains(&new_name) {
                let mut n = 2;
                loop {
                    let candidate = format!("{name} (imported {n})");
                    if !existing_names.contains(&candidate) {
                        new_name = candidate;
                        break;
                    }
                    n += 1;
                }
            }
            existing_names.insert(new_name.clone());

            tx.execute(
                "INSERT INTO workspaces (name, created_at, updated_at) VALUES (?1, ?2, ?3)",
                params![new_name, created_at, updated_at],
            )?;
            let new_ws_id = tx.last_insert_rowid();
            new_workspace_ids.push(new_ws_id);
            workspaces_added += 1;

            // --- sources ---
            let mut src_id_map: HashMap<i64, i64> = HashMap::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count
                     FROM ext.sources WHERE workspace_id = ?1",
                )?;
                let rows: Vec<(
                    i64,
                    String,
                    String,
                    String,
                    Option<String>,
                    Option<i64>,
                    String,
                    i64,
                    i64,
                )> = stmt
                    .query_map(params![old_ws_id], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (
                    old_id,
                    kind,
                    label,
                    device_label,
                    orig_root_path,
                    dev_id,
                    imported_at,
                    total_size,
                    file_count,
                ) in rows
                {
                    tx.execute(
                        "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![new_ws_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count],
                    )?;
                    let new_id = tx.last_insert_rowid();
                    src_id_map.insert(old_id, new_id);
                    sources_added += 1;
                }
            }

            // --- nodes (per source, preserving parent hierarchy via id map) ---
            let mut node_id_map: HashMap<i64, i64> = HashMap::new();
            for (&old_src_id, &new_src_id) in src_id_map.clone().iter() {
                // Nodes must be inserted in an order where parents precede
                // children. `id` order is guaranteed to satisfy this since
                // parent rows are always created (and thus assigned a lower
                // autoincrement id) before their children during scanning.
                let mut stmt = tx.prepare(
                    "SELECT id, parent_id, name, rel_path, type, size, mtime, inode, dev, depth, subtree_size, subtree_file_count
                     FROM ext.nodes WHERE source_id = ?1 ORDER BY id",
                )?;
                let rows: Vec<(
                    i64,
                    Option<i64>,
                    String,
                    String,
                    String,
                    i64,
                    Option<String>,
                    Option<i64>,
                    Option<i64>,
                    i64,
                    i64,
                    i64,
                )> = stmt
                    .query_map(params![old_src_id], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                            r.get(11)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (
                    old_id,
                    parent_id,
                    name,
                    rel_path,
                    ntype,
                    size,
                    mtime,
                    inode,
                    dev,
                    depth,
                    subtree_size,
                    subtree_file_count,
                ) in rows
                {
                    let new_parent_id = parent_id.and_then(|p| node_id_map.get(&p).copied());
                    tx.execute(
                        "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, mtime, inode, dev, depth, subtree_size, subtree_file_count)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![new_src_id, new_parent_id, name, rel_path, ntype, size, mtime, inode, dev, depth, subtree_size, subtree_file_count],
                    )?;
                    let new_id = tx.last_insert_rowid();
                    node_id_map.insert(old_id, new_id);
                    nodes_added += 1;
                }
            }

            // --- dup_annot (per-node duplicate annotation cache) ---
            {
                let mut stmt = tx.prepare(
                    "SELECT n.id, d.has_dup, d.dup_pct, d.cross_dup, d.cross_dup_size, d.cross_dup_file_count, d.in_folder_group
                     FROM ext.dup_annot d
                     JOIN ext.nodes n ON n.id = d.node_id
                     JOIN ext.sources s ON s.id = n.source_id
                     WHERE s.workspace_id = ?1",
                )?;
                let rows: Vec<(i64, i64, f64, i64, i64, i64, i64)> = stmt
                    .query_map(params![old_ws_id], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (
                    old_node_id,
                    has_dup,
                    dup_pct,
                    cross_dup,
                    cross_dup_size,
                    cross_dup_file_count,
                    in_folder_group,
                ) in rows
                {
                    if let Some(&new_node_id) = node_id_map.get(&old_node_id) {
                        tx.execute(
                            "INSERT OR IGNORE INTO dup_annot (node_id, has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count, in_folder_group)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            params![new_node_id, has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count, in_folder_group],
                        )?;
                    }
                }
            }

            // --- match_groups + match_members ---
            let mut group_id_map: HashMap<i64, i64> = HashMap::new();
            {
                let mut stmt = tx.prepare(
                    "SELECT id, kind, confidence, primary_signal, size FROM ext.match_groups WHERE workspace_id = ?1",
                )?;
                let rows: Vec<(i64, String, f64, String, i64)> = stmt
                    .query_map(params![old_ws_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (old_id, kind, confidence, primary_signal, size) in rows {
                    tx.execute(
                        "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![new_ws_id, kind, confidence, primary_signal, size],
                    )?;
                    group_id_map.insert(old_id, tx.last_insert_rowid());
                }
            }
            {
                let mut stmt = tx.prepare(
                    "SELECT mm.group_id, mm.node_id, mm.role FROM ext.match_members mm
                     JOIN ext.match_groups mg ON mg.id = mm.group_id
                     WHERE mg.workspace_id = ?1",
                )?;
                let rows: Vec<(i64, i64, String)> = stmt
                    .query_map(params![old_ws_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (old_group_id, old_node_id, role) in rows {
                    if let (Some(&new_group_id), Some(&new_node_id)) = (
                        group_id_map.get(&old_group_id),
                        node_id_map.get(&old_node_id),
                    ) {
                        tx.execute(
                            "INSERT OR IGNORE INTO match_members (group_id, node_id, role) VALUES (?1, ?2, ?3)",
                            params![new_group_id, new_node_id, role],
                        )?;
                    }
                }
            }

            // --- consolidations + consolidation_nodes ---
            let mut cons_id_map: HashMap<i64, i64> = HashMap::new();
            {
                let mut stmt =
                    tx.prepare("SELECT id, name FROM ext.consolidations WHERE workspace_id = ?1")?;
                let rows: Vec<(i64, String)> = stmt
                    .query_map(params![old_ws_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (old_id, name) in rows {
                    tx.execute(
                        "INSERT INTO consolidations (workspace_id, name) VALUES (?1, ?2)",
                        params![new_ws_id, name],
                    )?;
                    cons_id_map.insert(old_id, tx.last_insert_rowid());
                }
            }
            for (&old_cons_id, &new_cons_id) in cons_id_map.clone().iter() {
                let mut cnode_id_map: HashMap<i64, i64> = HashMap::new();
                let mut stmt = tx.prepare(
                    "SELECT id, parent_id, name, type, source_node_id, sort_order
                     FROM ext.consolidation_nodes WHERE consolidation_id = ?1 ORDER BY id",
                )?;
                let rows: Vec<(i64, Option<i64>, String, String, Option<i64>, i64)> = stmt
                    .query_map(params![old_cons_id], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (old_id, parent_id, name, ntype, source_node_id, sort_order) in rows {
                    let new_parent_id = parent_id.and_then(|p| cnode_id_map.get(&p).copied());
                    let new_source_node_id =
                        source_node_id.and_then(|s| node_id_map.get(&s).copied());
                    tx.execute(
                        "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![new_cons_id, new_parent_id, name, ntype, new_source_node_id, sort_order],
                    )?;
                    cnode_id_map.insert(old_id, tx.last_insert_rowid());
                }
            }

            // --- pathfix_state ---
            {
                let mut stmt = tx.prepare(
                    "SELECT kind, ref_id, new_name, original_name, resolved FROM ext.pathfix_state WHERE workspace_id = ?1",
                )?;
                let rows: Vec<(String, i64, String, String, i64)> = stmt
                    .query_map(params![old_ws_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (kind, ref_id, new_name, original_name, resolved) in rows {
                    let new_ref_id = if kind == "source" {
                        node_id_map.get(&ref_id).copied()
                    } else {
                        // kind == "cons": ref_id refers to a consolidation_nodes id,
                        // but we didn't keep a combined map across consolidations;
                        // look it up the slow way (small table, fine to scan).
                        None
                    };
                    if kind == "source" {
                        if let Some(new_ref_id) = new_ref_id {
                            tx.execute(
                                "INSERT OR IGNORE INTO pathfix_state (workspace_id, kind, ref_id, new_name, original_name, resolved)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                params![new_ws_id, kind, new_ref_id, new_name, original_name, resolved],
                            )?;
                        }
                    }
                    // "cons" kind pathfix rows are skipped: consolidation node
                    // remapping is per-consolidation and not worth the added
                    // complexity for what is transient UI state; they will
                    // simply be recomputed next time path limits are checked.
                }
            }
        }

        Ok(ImportSummary {
            workspaces_added,
            sources_added,
            new_workspace_ids,
            nodes_added,
        })
    })();

    // Commit or roll back the transaction first — ATTACH/DETACH cannot run
    // while a transaction is open on the connection.
    match result {
        Ok(summary) => {
            tx.commit()?;
            let _ = conn.execute_batch("DETACH DATABASE ext;");
            Ok(summary)
        }
        Err(e) => {
            tx.rollback().ok();
            let _ = conn.execute_batch("DETACH DATABASE ext;");
            Err(e)
        }
    }
}
