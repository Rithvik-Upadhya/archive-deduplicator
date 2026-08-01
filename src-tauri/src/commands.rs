//! Tauri command surface. Every mutating command writes through to SQLite so the
//! full application state is restored on next launch.

use crate::db::Db;
use crate::dedup::DedupParams;
use crate::model::*;
use crate::{parse, pathfix, rollup, scan};
use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use tauri::{Emitter, State};

type CmdResult<T> = Result<T, String>;

fn now() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn map_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn workspace_list(db: State<Db>) -> CmdResult<Vec<Workspace>> {
    let conn = db.0.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT id, name, created_at, updated_at FROM workspaces ORDER BY id")
        .map_err(map_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })
        .map_err(map_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(map_err)
}

#[tauri::command]
pub fn workspace_create(db: State<Db>, name: String) -> CmdResult<Workspace> {
    let conn = db.0.lock().unwrap();
    let ts = now();
    conn.execute(
        "INSERT INTO workspaces (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
        params![name, ts],
    )
    .map_err(map_err)?;
    let id = conn.last_insert_rowid();
    Ok(Workspace {
        id,
        name,
        created_at: ts.clone(),
        updated_at: ts,
    })
}

#[tauri::command]
pub fn workspace_rename(db: State<Db>, id: i64, name: String) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE workspaces SET name = ?1, updated_at = ?2 WHERE id = ?3",
        params![name, now(), id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn workspace_delete(db: State<Db>, id: i64) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute("DELETE FROM workspaces WHERE id = ?1", params![id])
        .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sources (devices)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn source_list(db: State<Db>, workspace_id: i64) -> CmdResult<Vec<Source>> {
    let conn = db.0.lock().unwrap();
    let dup_by_src = rollup::duplicated_size_by_source(&conn, workspace_id).map_err(map_err)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count
             FROM sources WHERE workspace_id = ?1 ORDER BY id",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![workspace_id], |r| {
            Ok(Source {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                kind: r.get(2)?,
                label: r.get(3)?,
                device_label: r.get(4)?,
                orig_root_path: r.get(5)?,
                dev_id: r.get(6)?,
                imported_at: r.get(7)?,
                total_size: r.get(8)?,
                file_count: r.get(9)?,
                duplicated_pct: 0.0,
            })
        })
        .map_err(map_err)?;
    let mut sources = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    for s in &mut sources {
        let dup = *dup_by_src.get(&s.id).unwrap_or(&0);
        s.duplicated_pct = if s.total_size > 0 {
            dup as f64 / s.total_size as f64 * 100.0
        } else {
            0.0
        };
    }
    Ok(sources)
}

/// Import a `tree` JSON document as a new source, flattening it into `nodes`.
#[tauri::command]
pub fn import_tree_json(
    db: State<Db>,
    workspace_id: i64,
    json_text: String,
    label: String,
) -> CmdResult<Source> {
    let flat = parse::parse_tree_json(&json_text)?;
    let mut conn = db.0.lock().unwrap();
    let ts = now();
    let device_label = label.clone();
    let tx = conn.transaction().map_err(map_err)?;
    tx.execute(
        "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count)
         VALUES (?1, 'json', ?2, ?3, NULL, ?4, ?5, ?6, ?7)",
        params![workspace_id, label, device_label, flat.root_dev, ts, flat.total_size, flat.file_count],
    )
    .map_err(map_err)?;
    let source_id = tx.last_insert_rowid();
    parse::insert_nodes(&tx, source_id, &flat).map_err(map_err)?;
    tx.commit().map_err(map_err)?;

    Ok(Source {
        id: source_id,
        workspace_id,
        kind: "json".into(),
        label,
        device_label,
        orig_root_path: None,
        dev_id: flat.root_dev,
        imported_at: ts,
        total_size: flat.total_size,
        file_count: flat.file_count,
        duplicated_pct: 0.0,
    })
}

/// Scan a local folder as a new source (works without the `tree` binary).
#[tauri::command]
pub fn scan_folder(
    db: State<Db>,
    workspace_id: i64,
    path: String,
    label: String,
) -> CmdResult<Source> {
    let flat = scan::scan_folder(Path::new(&path))?;
    let mut conn = db.0.lock().unwrap();
    let ts = now();
    let device_label = label.clone();
    let tx = conn.transaction().map_err(map_err)?;
    tx.execute(
        "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count)
         VALUES (?1, 'scan', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![workspace_id, label, device_label, path, flat.root_dev, ts, flat.total_size, flat.file_count],
    )
    .map_err(map_err)?;
    let source_id = tx.last_insert_rowid();
    parse::insert_nodes(&tx, source_id, &flat).map_err(map_err)?;
    tx.commit().map_err(map_err)?;

    Ok(Source {
        id: source_id,
        workspace_id,
        kind: "scan".into(),
        label,
        device_label,
        orig_root_path: Some(path),
        dev_id: flat.root_dev,
        imported_at: ts,
        total_size: flat.total_size,
        file_count: flat.file_count,
        duplicated_pct: 0.0,
    })
}

#[tauri::command]
pub fn source_rename_device(db: State<Db>, source_id: i64, device_label: String) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE sources SET device_label = ?1 WHERE id = ?2",
        params![device_label, source_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn source_delete(db: State<Db>, source_id: i64) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute("DELETE FROM sources WHERE id = ?1", params![source_id])
        .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree browsing (lazy)
// ---------------------------------------------------------------------------

/// Fetch the direct children of a node (or the roots of a source when
/// `parent_id` is null). Duplicate annotations come from the precomputed
/// `dup_annot` cache (rebuilt on every dedup run) so this stays fast even on
/// very large workspaces.
#[tauri::command]
pub fn get_tree(
    db: State<Db>,
    workspace_id: i64,
    source_id: i64,
    parent_id: Option<i64>,
) -> CmdResult<Vec<Node>> {
    let _ = workspace_id;
    let conn = db.0.lock().unwrap();

    let sql = if parent_id.is_some() {
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.rel_path, n.type, n.size, n.mtime, n.inode, n.dev, n.depth, n.subtree_size, n.subtree_file_count,
                COALESCE(d.has_dup, 0), COALESCE(d.dup_pct, 0)
         FROM nodes n LEFT JOIN dup_annot d ON d.node_id = n.id
         WHERE n.source_id = ?1 AND n.parent_id = ?2 ORDER BY n.type = 'file', n.name"
    } else {
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.rel_path, n.type, n.size, n.mtime, n.inode, n.dev, n.depth, n.subtree_size, n.subtree_file_count,
                COALESCE(d.has_dup, 0), COALESCE(d.dup_pct, 0)
         FROM nodes n LEFT JOIN dup_annot d ON d.node_id = n.id
         WHERE n.source_id = ?1 AND n.parent_id IS NULL ORDER BY n.type = 'file', n.name"
    };
    let mut stmt = conn.prepare(sql).map_err(map_err)?;

    let mapper = |r: &rusqlite::Row| -> rusqlite::Result<Node> {
        Ok(Node {
            id: r.get(0)?,
            source_id: r.get(1)?,
            parent_id: r.get(2)?,
            name: r.get(3)?,
            rel_path: r.get(4)?,
            node_type: r.get(5)?,
            size: r.get(6)?,
            mtime: r.get(7)?,
            inode: r.get(8)?,
            dev: r.get(9)?,
            depth: r.get(10)?,
            subtree_size: r.get(11)?,
            subtree_file_count: r.get(12)?,
            has_duplicate: r.get::<_, i64>(13)? != 0,
            dup_pct: r.get(14)?,
        })
    };

    let rows = if let Some(pid) = parent_id {
        stmt.query_map(params![source_id, pid], mapper)
    } else {
        stmt.query_map(params![source_id], mapper)
    }
    .map_err(map_err)?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(map_err)
}

// ---------------------------------------------------------------------------
// Dedup
// ---------------------------------------------------------------------------

/// Run the full duplicate-detection pass with the given tuning parameters.
#[tauri::command]
pub fn run_dedup(
    app: tauri::AppHandle,
    db: State<Db>,
    workspace_id: i64,
    min_size_bytes: i64,
    min_confidence: f64,
) -> CmdResult<usize> {
    let params = DedupParams {
        min_size_bytes,
        min_confidence,
    };
    let _ = app.emit(
        "dedup:progress",
        DedupProgress {
            phase: "matching".into(),
            current: 0,
            total: 1,
        },
    );
    let mut conn = db.0.lock().unwrap();
    let count = crate::dedup::run(&mut conn, workspace_id, params).map_err(map_err)?;
    let _ = app.emit(
        "dedup:progress",
        DedupProgress {
            phase: "done".into(),
            current: 1,
            total: 1,
        },
    );
    Ok(count)
}

/// Return a page of match groups for a workspace, filtered by confidence,
/// minimum size and kind, sorted by confidence or size. Members are fetched
/// with a single batched query per page so pagination stays cheap even with
/// hundreds of thousands of groups.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn get_groups(
    db: State<Db>,
    workspace_id: i64,
    min_confidence: f64,
    min_size: i64,
    kind: Option<String>,
    sort: Option<String>,
    offset: i64,
    limit: i64,
) -> CmdResult<GroupPage> {
    let conn = db.0.lock().unwrap();
    let kind_filter = kind.unwrap_or_default();
    let order = match sort.as_deref() {
        Some("size") => "size DESC, confidence DESC",
        _ => "confidence DESC, size DESC",
    };
    let limit = if limit <= 0 { 100 } else { limit.min(500) };

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM match_groups
             WHERE workspace_id = ?1 AND confidence >= ?2 AND size >= ?3
               AND (?4 = '' OR kind = ?4)",
            params![workspace_id, min_confidence, min_size, kind_filter],
            |r| r.get(0),
        )
        .map_err(map_err)?;

    let sql = format!(
        "SELECT id, workspace_id, kind, confidence, primary_signal, size
         FROM match_groups
         WHERE workspace_id = ?1 AND confidence >= ?2 AND size >= ?3
           AND (?4 = '' OR kind = ?4)
         ORDER BY {order} LIMIT ?5 OFFSET ?6"
    );
    let mut stmt = conn.prepare(&sql).map_err(map_err)?;
    let mut groups = stmt
        .query_map(
            params![
                workspace_id,
                min_confidence,
                min_size,
                kind_filter,
                limit,
                offset
            ],
            |r| {
                Ok(MatchGroup {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    kind: r.get(2)?,
                    confidence: r.get(3)?,
                    primary_signal: r.get(4)?,
                    size: r.get(5)?,
                    members: Vec::new(),
                })
            },
        )
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;

    // Batch-load members for this page of groups.
    if !groups.is_empty() {
        let ids: Vec<String> = groups.iter().map(|g| g.id.to_string()).collect();
        let sql = format!(
            "SELECT mm.group_id, n.id, n.source_id, s.device_label, n.rel_path, n.name, n.size, n.mtime
             FROM match_members mm
             JOIN nodes n ON n.id = mm.node_id
             JOIN sources s ON s.id = n.source_id
             WHERE mm.group_id IN ({})",
            ids.join(",")
        );
        let mut mstmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = mstmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    MatchMember {
                        node_id: r.get(1)?,
                        source_id: r.get(2)?,
                        device_label: r.get(3)?,
                        rel_path: r.get(4)?,
                        name: r.get(5)?,
                        size: r.get(6)?,
                        mtime: r.get(7)?,
                    },
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        let mut by_group: HashMap<i64, Vec<MatchMember>> = HashMap::new();
        for (gid, m) in rows {
            by_group.entry(gid).or_default().push(m);
        }
        for g in &mut groups {
            g.members = by_group.remove(&g.id).unwrap_or_default();
        }
    }

    Ok(GroupPage { total, groups })
}

/// Return the match group (with members) that a given node belongs to, if any.
/// Used by the "locate duplicate" button in the device trees.
#[tauri::command]
pub fn get_group_for_node(db: State<Db>, node_id: i64) -> CmdResult<Option<MatchGroup>> {
    let conn = db.0.lock().unwrap();
    let group: Option<MatchGroup> = conn
        .query_row(
            "SELECT mg.id, mg.workspace_id, mg.kind, mg.confidence, mg.primary_signal, mg.size
             FROM match_members mm JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mm.node_id = ?1 ORDER BY mg.confidence DESC LIMIT 1",
            params![node_id],
            |r| {
                Ok(MatchGroup {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    kind: r.get(2)?,
                    confidence: r.get(3)?,
                    primary_signal: r.get(4)?,
                    size: r.get(5)?,
                    members: Vec::new(),
                })
            },
        )
        .ok();
    let Some(mut g) = group else { return Ok(None) };
    let mut mstmt = conn
        .prepare(
            "SELECT n.id, n.source_id, s.device_label, n.rel_path, n.name, n.size, n.mtime
             FROM match_members mm
             JOIN nodes n ON n.id = mm.node_id
             JOIN sources s ON s.id = n.source_id
             WHERE mm.group_id = ?1",
        )
        .map_err(map_err)?;
    g.members = mstmt
        .query_map(params![g.id], |r| {
            Ok(MatchMember {
                node_id: r.get(0)?,
                source_id: r.get(1)?,
                device_label: r.get(2)?,
                rel_path: r.get(3)?,
                name: r.get(4)?,
                size: r.get(5)?,
                mtime: r.get(6)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok(Some(g))
}

/// Per-device duplicate statistics.
#[tauri::command]
pub fn get_device_stats(db: State<Db>, workspace_id: i64) -> CmdResult<Vec<DeviceStats>> {
    let conn = db.0.lock().unwrap();
    let dup_by_src = rollup::duplicated_size_by_source(&conn, workspace_id).map_err(map_err)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, device_label, total_size, file_count FROM sources WHERE workspace_id = ?1 ORDER BY id",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![workspace_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map_err(map_err)?;
    let mut out = Vec::new();
    for row in rows {
        let (id, label, total, count) = row.map_err(map_err)?;
        let dup = *dup_by_src.get(&id).unwrap_or(&0);
        out.push(DeviceStats {
            source_id: id,
            device_label: label,
            total_size: total,
            file_count: count,
            duplicated_size: dup,
            duplicated_pct: if total > 0 {
                dup as f64 / total as f64 * 100.0
            } else {
                0.0
            },
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Consolidation
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn consolidation_get(
    db: State<Db>,
    workspace_id: i64,
) -> CmdResult<(i64, Vec<ConsolidationNode>)> {
    let conn = db.0.lock().unwrap();
    // Ensure a single consolidation exists per workspace.
    let id: i64 = {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM consolidations WHERE workspace_id = ?1 LIMIT 1",
                params![workspace_id],
                |r| r.get(0),
            )
            .ok();
        match existing {
            Some(i) => i,
            None => {
                conn.execute(
                    "INSERT INTO consolidations (workspace_id, name) VALUES (?1, 'Consolidated')",
                    params![workspace_id],
                )
                .map_err(map_err)?;
                conn.last_insert_rowid()
            }
        }
    };
    let mut stmt = conn
        .prepare(
            "SELECT cn.id, cn.consolidation_id, cn.parent_id, cn.name, cn.type,
                    cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path
             FROM consolidation_nodes cn
             LEFT JOIN nodes n ON n.id = cn.source_node_id
             LEFT JOIN sources s ON s.id = n.source_id
             WHERE cn.consolidation_id = ?1
             ORDER BY cn.parent_id, cn.sort_order",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![id], |r| {
            let node_type: String = r.get(4)?;
            Ok(ConsolidationNode {
                id: r.get(0)?,
                consolidation_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                node_type: node_type.clone(),
                source_node_id: r.get(5)?,
                sort_order: r.get(6)?,
                size: if node_type == "directory" {
                    None
                } else {
                    r.get(7)?
                },
                origin_device: r.get(8)?,
                origin_path: r.get(9)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok((id, rows))
}

#[tauri::command]
pub fn consolidation_add_node(
    db: State<Db>,
    consolidation_id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    source_node_id: Option<i64>,
) -> CmdResult<ConsolidationNode> {
    let conn = db.0.lock().unwrap();
    let sort_order: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM consolidation_nodes
             WHERE consolidation_id = ?1 AND parent_id IS ?2",
            params![consolidation_id, parent_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![consolidation_id, parent_id, name, node_type, source_node_id, sort_order],
    )
    .map_err(map_err)?;
    let id = conn.last_insert_rowid();
    conn.query_row(
        "SELECT cn.id, cn.consolidation_id, cn.parent_id, cn.name, cn.type,
                cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path
         FROM consolidation_nodes cn
         LEFT JOIN nodes n ON n.id = cn.source_node_id
         LEFT JOIN sources s ON s.id = n.source_id
         WHERE cn.id = ?1",
        params![id],
        |r| {
            let node_type: String = r.get(4)?;
            Ok(ConsolidationNode {
                id: r.get(0)?,
                consolidation_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                node_type: node_type.clone(),
                source_node_id: r.get(5)?,
                sort_order: r.get(6)?,
                size: if node_type == "directory" {
                    None
                } else {
                    r.get(7)?
                },
                origin_device: r.get(8)?,
                origin_path: r.get(9)?,
            })
        },
    )
    .map_err(map_err)
}

/// Drag-drop a source *directory* wholesale: materializes its entire subtree
/// as real `consolidation_nodes` rows (root + every descendant), so every
/// file/folder inside becomes individually movable/renamable/deletable.
#[tauri::command]
pub fn consolidation_add_source_subtree(
    db: State<Db>,
    consolidation_id: i64,
    parent_id: Option<i64>,
    source_node_id: i64,
) -> CmdResult<Vec<ConsolidationNode>> {
    let mut conn = db.0.lock().unwrap();
    crate::consolidate::materialize_subtree(&mut conn, consolidation_id, parent_id, source_node_id)
        .map_err(map_err)
}

#[tauri::command]
pub fn consolidation_move_node(
    db: State<Db>,
    node_id: i64,
    parent_id: Option<i64>,
    sort_order: i64,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE consolidation_nodes SET parent_id = ?1, sort_order = ?2 WHERE id = ?3",
        params![parent_id, sort_order, node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn consolidation_rename_node(db: State<Db>, node_id: i64, name: String) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE consolidation_nodes SET name = ?1 WHERE id = ?2",
        params![name, node_id],
    )
    .map_err(map_err)?;
    // Keep the Path-limits view in sync: it prefers pathfix_state.new_name
    // over consolidation_nodes.name whenever a row exists (e.g. this node was
    // already renamed once from that view). This UPDATE is a no-op when no
    // such row exists yet, which is the common case.
    conn.execute(
        "UPDATE pathfix_state SET new_name = ?1 WHERE kind = 'cons' AND ref_id = ?2",
        params![name, node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn consolidation_delete_node(db: State<Db>, node_id: i64) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "DELETE FROM consolidation_nodes WHERE id = ?1",
        params![node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Path-limit view
// ---------------------------------------------------------------------------

/// Build the consolidated end-state tree annotated with path-length
/// guidance (step 3: dedup → consolidate → fix path limits). Every node is
/// returned, not just over-limit ones; `over_limit` marks every node on an
/// offending root-to-leaf chain.
#[tauri::command]
pub fn pathfix_tree(db: State<Db>, workspace_id: i64, limit: i64) -> CmdResult<Vec<PathTreeNode>> {
    let conn = db.0.lock().unwrap();
    pathfix::build_tree(&conn, workspace_id, limit).map_err(map_err)
}

/// Rename a node in the consolidated end-state tree. Empty name reverts the edit.
#[tauri::command]
pub fn pathfix_rename(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    new_name: String,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    pathfix::rename(&conn, workspace_id, node_id, &new_name).map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// App state (UI persistence)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn app_state_get(db: State<Db>, key: String) -> CmdResult<Option<String>> {
    let conn = db.0.lock().unwrap();
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM app_state WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .ok();
    Ok(v)
}

#[tauri::command]
pub fn app_state_set(db: State<Db>, key: String, value: String) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "INSERT INTO app_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Database export / import
// ---------------------------------------------------------------------------

/// Export the whole live database (every workspace) as a single SQLite file
/// at the user-chosen `path`.
#[tauri::command]
pub fn db_export(db: State<Db>, path: String) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    crate::dbio::export_to(&conn, Path::new(&path)).map_err(map_err)
}

/// Import a previously exported SQLite database file at `path`, merging its
/// workspaces (and everything under them) into the live database as new
/// workspaces. Nothing already present is modified or deleted.
#[tauri::command]
pub fn db_import(db: State<Db>, path: String) -> CmdResult<crate::dbio::ImportSummary> {
    let mut conn = db.0.lock().unwrap();
    crate::dbio::import_merge(&mut conn, Path::new(&path)).map_err(map_err)
}
