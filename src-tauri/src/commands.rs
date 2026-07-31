//! Tauri command surface. Every mutating command writes through to SQLite so the
//! full application state is restored on next launch.

use crate::db::Db;
use crate::dedup::DedupParams;
use crate::model::*;
use crate::{marks, parse, pathfix, rollup, scan};
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

    log_action(
        &conn,
        workspace_id,
        "import_json",
        &format!("Imported '{label}' ({} files)", flat.file_count),
    );

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

    log_action(
        &conn,
        workspace_id,
        "scan_folder",
        &format!("Scanned '{path}' as '{label}' ({} files)", flat.file_count),
    );

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
    invalidate_scope_cache(&conn);
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
    // Group ids are rebuilt from scratch, so any cached scope is now wrong.
    invalidate_scope_cache(&conn);
    log_action(
        &conn,
        workspace_id,
        "run_dedup",
        &format!("Found {count} duplicate groups"),
    );
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

/// Identifies a subtree the group list is scoped to.
struct Scope {
    source_id: i64,
    rel_path: String,
}

impl Scope {
    fn load(conn: &rusqlite::Connection, node_id: i64) -> CmdResult<Scope> {
        conn.query_row(
            "SELECT source_id, rel_path FROM nodes WHERE id = ?1",
            params![node_id],
            |r| {
                Ok(Scope {
                    source_id: r.get(0)?,
                    rel_path: r.get(1)?,
                })
            },
        )
        .map_err(|_| format!("node {node_id} not found"))
    }

    /// Whether a node lies at or beneath the scope root.
    fn contains(&self, source_id: i64, rel_path: &str) -> bool {
        source_id == self.source_id
            && (rel_path == self.rel_path
                || rel_path
                    .strip_prefix(&self.rel_path)
                    .is_some_and(|rest| rest.starts_with('/')))
    }

    /// Materialize into `temp.scope_groups` every group with a member inside
    /// this subtree, along with the largest such member's size.
    ///
    /// The direction matters enormously. Expressed as an `EXISTS` correlated to
    /// each candidate group, SQLite drives the subquery from `nodes` and
    /// re-scans the whole subtree once per group — 249k rows × 9.9k groups,
    /// which reads as a hang. Collecting the group ids once, driven from the
    /// `idx_nodes_path` range scan, takes ~100ms on the same data.
    fn materialize(&self, conn: &rusqlite::Connection) -> CmdResult<()> {
        // Scrolling through a large folder must not re-walk its subtree for
        // every page, so the result is kept until the scope actually moves.
        let cached: Option<(i64, String)> = conn
            .query_row("SELECT source_id, rel_path FROM temp.scope_meta", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .ok();
        if cached.is_some_and(|(s, p)| s == self.source_id && p == self.rel_path) {
            return Ok(());
        }

        conn.execute_batch(
            "DROP TABLE IF EXISTS temp.scope_groups;
             DROP TABLE IF EXISTS temp.scope_meta;
             CREATE TEMP TABLE scope_groups (
                 group_id INTEGER PRIMARY KEY,
                 bytes INTEGER NOT NULL
             );
             CREATE TEMP TABLE scope_meta (source_id INTEGER, rel_path TEXT);",
        )
        .map_err(map_err)?;
        conn.execute(
            "INSERT INTO temp.scope_groups (group_id, bytes)
             SELECT mm.group_id, MAX(n.size)
             FROM nodes n
             JOIN match_members mm ON mm.node_id = n.id
             WHERE n.source_id = ?1
               AND (n.rel_path = ?2 OR n.rel_path LIKE ?2 || '/%')
             GROUP BY mm.group_id",
            params![self.source_id, self.rel_path],
        )
        .map_err(map_err)?;
        conn.execute(
            "INSERT INTO temp.scope_meta (source_id, rel_path) VALUES (?1, ?2)",
            params![self.source_id, self.rel_path],
        )
        .map_err(map_err)?;
        Ok(())
    }
}

/// Drop the cached scope, which is derived from groups and nodes and so must
/// not outlive a change to either.
fn invalidate_scope_cache(conn: &rusqlite::Connection) {
    let _ = conn.execute_batch(
        "DROP TABLE IF EXISTS temp.scope_groups;
         DROP TABLE IF EXISTS temp.scope_meta;",
    );
}

/// Return a page of match groups, filtered by confidence, minimum size, kind,
/// an optional subtree scope and an optional decision state; sorted by
/// confidence or size.
///
/// The scope is what makes a folder clickable: a folder showing "55% dup" is
/// almost never a member of a folder-level group itself — the duplication lives
/// in the files *inside* it — so scoping asks "which groups have a member under
/// here" rather than "which group is this node in".
///
/// Members are fetched with a single batched query per page so pagination stays
/// cheap even with hundreds of thousands of groups.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn get_groups(
    db: State<Db>,
    workspace_id: i64,
    min_confidence: f64,
    min_size: i64,
    kind: Option<String>,
    sort: Option<String>,
    scope_node_id: Option<i64>,
    decision: Option<String>,
    offset: i64,
    limit: i64,
) -> CmdResult<GroupPage> {
    let conn = db.0.lock().unwrap();
    let kind_filter = kind.unwrap_or_default();
    // Tiebreak on id: thousands of groups share the same (confidence, size), and
    // without a total order LIMIT/OFFSET pages silently skip and repeat rows.
    let order = match sort.as_deref() {
        Some("size") => "size DESC, confidence DESC, id DESC",
        _ => "confidence DESC, size DESC, id DESC",
    };
    let limit = if limit <= 0 { 100 } else { limit.min(500) };

    let scope = match scope_node_id {
        Some(id) => Some(Scope::load(&conn, id)?),
        None => None,
    };

    let mut preds = String::new();
    if let Some(s) = &scope {
        s.materialize(&conn)?;
        preds.push_str(" AND mg.id IN (SELECT group_id FROM temp.scope_groups)");
    }

    // Resolve decisions once into an indexed temp table. With nothing marked
    // yet every group is undecided and the join can be skipped entirely.
    let resolver = marks::Resolver::load(&conn, workspace_id).map_err(map_err)?;
    let has_marks = resolver
        .materialize_keep_set(&conn, workspace_id)
        .map_err(map_err)?;
    let decision_filter = decision.as_deref().filter(|d| *d != "all");
    match decision_filter {
        Some("undecided") if has_marks => {
            preds.push_str(&format!(" AND {} = 0", marks::keep_count_sql("mg")));
        }
        Some("decided") if has_marks => {
            preds.push_str(&format!(" AND {} = 1", marks::keep_count_sql("mg")));
        }
        Some("conflict") if has_marks => {
            preds.push_str(&format!(" AND {} > 1", marks::keep_count_sql("mg")));
        }
        // Nothing is decided or conflicting on a workspace with no marks.
        Some("decided") | Some("conflict") => preds.push_str(" AND 0"),
        _ => {}
    }

    let base = format!(
        "FROM match_groups mg
         WHERE mg.workspace_id = :ws AND mg.confidence >= :minconf AND mg.size >= :minsize
           AND (:kind = '' OR mg.kind = :kind){preds}"
    );

    let named: &[(&str, &dyn rusqlite::ToSql)] = &[
        (":ws", &workspace_id),
        (":minconf", &min_confidence),
        (":minsize", &min_size),
        (":kind", &kind_filter),
    ];

    let total: i64 = conn
        .query_row(&format!("SELECT COUNT(*) {base}"), named, |r| r.get(0))
        .map_err(map_err)?;

    let sql = format!(
        "SELECT mg.id, mg.workspace_id, mg.kind, mg.confidence, mg.primary_signal, mg.size
         {base} ORDER BY {order} LIMIT :limit OFFSET :offset"
    );
    let mut page_params = named.to_vec();
    page_params.push((":limit", &limit));
    page_params.push((":offset", &offset));

    let mut stmt = conn.prepare(&sql).map_err(map_err)?;
    let mut groups = stmt
        .query_map(page_params.as_slice(), |r| {
            Ok(MatchGroup {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                kind: r.get(2)?,
                confidence: r.get(3)?,
                primary_signal: r.get(4)?,
                size: r.get(5)?,
                members: Vec::new(),
                decision: String::new(),
                keeper_node_id: None,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;

    // Batch-load members for this page of groups.
    if !groups.is_empty() {
        let ids: Vec<String> = groups.iter().map(|g| g.id.to_string()).collect();
        let mut by_group =
            load_members(&conn, &ids.join(","), scope.as_ref(), &resolver)?;
        for g in &mut groups {
            g.members = by_group.remove(&g.id).unwrap_or_default();
            let d = marks::decide(&g.members);
            g.decision = d.as_str().to_string();
            g.keeper_node_id = d.keeper();
        }
    }

    Ok(GroupPage { total, groups })
}

/// Load the members of an already-selected set of group ids, annotated with
/// their effective mark and whether they fall inside the active scope.
/// `group_ids` is a comma-joined list of ids read straight from the database,
/// so it needs no further escaping.
fn load_members(
    conn: &rusqlite::Connection,
    group_ids: &str,
    scope: Option<&Scope>,
    resolver: &marks::Resolver,
) -> CmdResult<HashMap<i64, Vec<MatchMember>>> {
    let sql = format!(
        "SELECT mm.group_id, n.id, n.source_id, s.device_label, n.rel_path, n.name,
                n.type, n.size, n.mtime
         FROM match_members mm
         JOIN nodes n ON n.id = mm.node_id
         JOIN sources s ON s.id = n.source_id
         WHERE mm.group_id IN ({group_ids})"
    );
    let mut stmt = conn.prepare(&sql).map_err(map_err)?;
    let rows = stmt
        .query_map([], |r| {
            let source_id: i64 = r.get(2)?;
            let rel_path: String = r.get(4)?;
            let in_scope = scope.is_none_or(|s| s.contains(source_id, &rel_path));
            let eff = resolver.effective(source_id, &rel_path);
            Ok((
                r.get::<_, i64>(0)?,
                MatchMember {
                    node_id: r.get(1)?,
                    source_id,
                    device_label: r.get(3)?,
                    rel_path,
                    name: r.get(5)?,
                    node_type: r.get(6)?,
                    size: r.get(7)?,
                    mtime: r.get(8)?,
                    mark: eff.map(|(m, _)| m.as_str().to_string()),
                    mark_explicit: eff.is_some_and(|(_, explicit)| explicit),
                    in_scope,
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
    // Keep in-scope copies first so the user reads their own side of the match
    // before its counterparts.
    for members in by_group.values_mut() {
        members.sort_by(|a, b| {
            b.in_scope
                .cmp(&a.in_scope)
                .then_with(|| a.device_label.cmp(&b.device_label))
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });
    }
    Ok(by_group)
}

/// Summarize the duplication inside one node's subtree: how much is duplicated,
/// which folders elsewhere hold the counterparts, and how many groups still
/// need a decision. Drives the scope card in the dedup view.
#[tauri::command]
pub fn get_folder_report(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    limit: i64,
) -> CmdResult<FolderReport> {
    let conn = db.0.lock().unwrap();
    let limit = if limit <= 0 { 8 } else { limit.min(50) };

    let (source_id, device_label, name, rel_path, node_type, size, subtree_size, file_count, dup_pct): (
        i64,
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        f64,
    ) = conn
        .query_row(
            "SELECT n.source_id, s.device_label, n.name, n.rel_path, n.type, n.size,
                    n.subtree_size, n.subtree_file_count, COALESCE(d.dup_pct, 0)
             FROM nodes n
             JOIN sources s ON s.id = n.source_id
             LEFT JOIN dup_annot d ON d.node_id = n.id
             WHERE n.id = ?1",
            params![node_id],
            |r| {
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
            },
        )
        .map_err(|_| format!("node {node_id} not found"))?;

    let is_dir = node_type == "directory";
    let total_size = if is_dir { subtree_size } else { size };
    let file_count = if is_dir { file_count } else { 1 };

    // Collect the groups represented inside this subtree once; every figure
    // below reads from that temp table rather than re-walking the subtree.
    let scope = Scope {
        source_id,
        rel_path: rel_path.clone(),
    };
    scope.materialize(&conn)?;

    let (group_count, dup_bytes): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(bytes), 0) FROM temp.scope_groups",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(map_err)?;

    // Skip the decision join entirely until decisions exist.
    let resolver = marks::Resolver::load(&conn, workspace_id).map_err(map_err)?;
    let has_marks = resolver
        .materialize_keep_set(&conn, workspace_id)
        .map_err(map_err)?;
    let (decided_count, conflict_count) = if group_count == 0 || !has_marks {
        (0, 0)
    } else {
        let keep = marks::keep_count_sql("mg");
        let sql = format!(
            "SELECT COALESCE(SUM({keep} = 1), 0), COALESCE(SUM({keep} > 1), 0)
             FROM match_groups mg
             WHERE mg.id IN (SELECT group_id FROM temp.scope_groups)"
        );
        conn.query_row(&sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(map_err)?
    };

    // Rank the folders holding the counterparts. Attribution goes to each
    // partner file's parent directory: that is the level a user actually acts
    // on, and it keeps the list short without guessing at a common ancestor.
    let overlaps = if group_count == 0 {
        Vec::new()
    } else {
        let mut stmt = conn
            .prepare(
                "SELECT p.id, p.source_id, s.device_label, p.rel_path, p.name,
                        SUM(n.size) AS shared, COUNT(*) AS files
                 FROM temp.scope_groups sg
                 JOIN match_members mm ON mm.group_id = sg.group_id
                 JOIN nodes n ON n.id = mm.node_id
                 JOIN nodes p ON p.id = n.parent_id
                 JOIN sources s ON s.id = n.source_id
                 WHERE NOT (n.source_id = ?1
                            AND (n.rel_path = ?2 OR n.rel_path LIKE ?2 || '/%'))
                 GROUP BY p.id
                 ORDER BY shared DESC
                 LIMIT ?3",
            )
            .map_err(map_err)?;
        stmt.query_map(params![source_id, rel_path, limit], |r| {
            Ok(FolderOverlap {
                node_id: r.get(0)?,
                source_id: r.get(1)?,
                device_label: r.get(2)?,
                rel_path: r.get(3)?,
                name: r.get(4)?,
                shared_bytes: r.get(5)?,
                shared_files: r.get(6)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?
    };

    Ok(FolderReport {
        node_id,
        source_id,
        device_label,
        name,
        rel_path,
        node_type,
        total_size,
        file_count,
        dup_pct,
        dup_bytes,
        group_count,
        decided_count,
        conflict_count,
        overlaps,
    })
}

// ---------------------------------------------------------------------------
// Keeper decisions
// ---------------------------------------------------------------------------

/// Every explicit mark in the workspace. Small (one row per user decision), so
/// the frontend loads them all and applies subtree inheritance when rendering.
#[tauri::command]
pub fn get_marks(db: State<Db>, workspace_id: i64) -> CmdResult<Vec<NodeMark>> {
    let conn = db.0.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT nm.node_id, n.source_id, n.rel_path, n.type, nm.mark
             FROM node_marks nm JOIN nodes n ON n.id = nm.node_id
             WHERE nm.workspace_id = ?1",
        )
        .map_err(map_err)?;
    stmt.query_map(params![workspace_id], |r| {
        Ok(NodeMark {
            node_id: r.get(0)?,
            source_id: r.get(1)?,
            rel_path: r.get(2)?,
            node_type: r.get(3)?,
            mark: r.get(4)?,
        })
    })
    .map_err(map_err)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(map_err)
}

/// Mark a node as the definitive copy (`keep`), as surplus (`drop`), or clear
/// its mark (`None`). A mark on a directory covers its whole subtree.
#[tauri::command]
pub fn set_node_mark(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    mark: Option<String>,
) -> CmdResult<()> {
    let parsed = match mark.as_deref() {
        None | Some("") => None,
        Some(m) => Some(marks::Mark::parse(m).ok_or_else(|| format!("unknown mark: {m}"))?),
    };
    let conn = db.0.lock().unwrap();
    marks::set_mark(&conn, workspace_id, node_id, parsed, &now()).map_err(map_err)?;
    let detail = match parsed {
        Some(m) => format!("node {node_id} marked {}", m.as_str()),
        None => format!("node {node_id} decision cleared"),
    };
    log_action(&conn, workspace_id, "mark_node", &detail);
    Ok(())
}

/// Mark one member of a group as the copy to keep, clearing explicit marks on
/// its siblings so the group lands on exactly one keeper. Passing a `node_id`
/// that is already the keeper clears the decision, making the star a toggle.
#[tauri::command]
pub fn set_group_keeper(
    db: State<Db>,
    workspace_id: i64,
    group_id: i64,
    node_id: Option<i64>,
) -> CmdResult<()> {
    let mut conn = db.0.lock().unwrap();
    let members: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT node_id FROM match_members WHERE group_id = ?1")
            .map_err(map_err)?;
        stmt.query_map(params![group_id], |r| r.get(0))
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?
    };
    if let Some(id) = node_id {
        if !members.contains(&id) {
            return Err(format!("node {id} is not a member of group {group_id}"));
        }
    }

    let now = now();
    let tx = conn.transaction().map_err(map_err)?;
    for m in &members {
        // Siblings get an explicit `drop` so the decision survives even if an
        // ancestor is later marked `keep` for unrelated reasons.
        let mark = match node_id {
            Some(id) if *m == id => Some(marks::Mark::Keep),
            Some(_) => Some(marks::Mark::Drop),
            None => None,
        };
        marks::set_mark(&tx, workspace_id, *m, mark, &now).map_err(map_err)?;
    }
    tx.commit().map_err(map_err)?;

    let detail = match node_id {
        Some(id) => format!("group {group_id}: keeping node {id}"),
        None => format!("group {group_id}: decision cleared"),
    };
    log_action(&conn, workspace_id, "mark_keeper", &detail);
    Ok(())
}

/// Clear every decision at or beneath a node.
#[tauri::command]
pub fn clear_marks(db: State<Db>, workspace_id: i64, node_id: i64) -> CmdResult<usize> {
    let conn = db.0.lock().unwrap();
    let n = marks::clear_subtree(&conn, workspace_id, node_id).map_err(map_err)?;
    log_action(
        &conn,
        workspace_id,
        "mark_node",
        &format!("cleared {n} decisions under node {node_id}"),
    );
    Ok(n)
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
            "SELECT id, consolidation_id, parent_id, name, type, source_node_id, action, sort_order
             FROM consolidation_nodes WHERE consolidation_id = ?1 ORDER BY parent_id, sort_order",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![id], |r| {
            Ok(ConsolidationNode {
                id: r.get(0)?,
                consolidation_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                node_type: r.get(4)?,
                source_node_id: r.get(5)?,
                action: r.get(6)?,
                sort_order: r.get(7)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok((id, rows))
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn consolidation_add_node(
    db: State<Db>,
    workspace_id: i64,
    consolidation_id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    source_node_id: Option<i64>,
    action: String,
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
        "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, action, sort_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![consolidation_id, parent_id, name, node_type, source_node_id, action, sort_order],
    )
    .map_err(map_err)?;
    let id = conn.last_insert_rowid();
    log_action(
        &conn,
        workspace_id,
        "consolidate_add",
        &format!("Added '{name}' ({action}) to consolidation"),
    );
    Ok(ConsolidationNode {
        id,
        consolidation_id,
        parent_id,
        name,
        node_type,
        source_node_id,
        action,
        sort_order,
    })
}

#[tauri::command]
pub fn consolidation_set_action(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    action: String,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE consolidation_nodes SET action = ?1 WHERE id = ?2",
        params![action, node_id],
    )
    .map_err(map_err)?;
    log_action(
        &conn,
        workspace_id,
        "consolidate_action",
        &format!("Set node {node_id} action to {action}"),
    );
    Ok(())
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
pub fn consolidation_rename_node(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    name: String,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    conn.execute(
        "UPDATE consolidation_nodes SET name = ?1 WHERE id = ?2",
        params![name, node_id],
    )
    .map_err(map_err)?;
    log_action(
        &conn,
        workspace_id,
        "consolidate_rename",
        &format!("Renamed consolidation node to '{name}'"),
    );
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
// Action log
// ---------------------------------------------------------------------------

fn log_action(conn: &rusqlite::Connection, workspace_id: i64, op: &str, detail: &str) {
    let _ = conn.execute(
        "INSERT INTO action_log (workspace_id, ts, op, detail) VALUES (?1, ?2, ?3, ?4)",
        params![workspace_id, now(), op, detail],
    );
}

#[tauri::command]
pub fn action_log_list(
    db: State<Db>,
    workspace_id: i64,
    op_prefix: Option<String>,
) -> CmdResult<Vec<ActionLogEntry>> {
    let conn = db.0.lock().unwrap();
    let prefix = format!("{}%", op_prefix.unwrap_or_default());
    let mut stmt = conn
        .prepare(
            "SELECT id, workspace_id, ts, op, detail FROM action_log
             WHERE workspace_id = ?1 AND op LIKE ?2 ORDER BY id DESC LIMIT 500",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![workspace_id, prefix], |r| {
            Ok(ActionLogEntry {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                ts: r.get(2)?,
                op: r.get(3)?,
                detail: r.get(4)?,
            })
        })
        .map_err(map_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(map_err)
}

/// Render the consolidation plan as an **end-state guide**: the final target
/// tree plus a flat list of concrete steps (create folder / move / copy /
/// skip). Because it is derived from the current tree — not from the change
/// history — moves that cancelled each other out never appear.
#[tauri::command]
pub fn export_action_log(db: State<Db>, workspace_id: i64) -> CmdResult<String> {
    let conn = db.0.lock().unwrap();
    let mut out = String::from("# Archive Deduplicator — Consolidation Guide\n\n");
    out.push_str(
        "This guide describes the desired END STATE only. Perform the steps in\norder; intermediate moves made while planning have already been folded in.\n\n",
    );

    let cid: Option<i64> = conn
        .query_row(
            "SELECT id FROM consolidations WHERE workspace_id = ?1 LIMIT 1",
            params![workspace_id],
            |r| r.get(0),
        )
        .ok();
    let Some(cid) = cid else {
        out.push_str("_No consolidation tree defined._\n");
        return Ok(out);
    };

    // Load the whole consolidation tree.
    struct CNode {
        id: i64,
        parent_id: Option<i64>,
        name: String,
        node_type: String,
        action: String,
        source_node_id: Option<i64>,
        sort_order: i64,
    }
    let mut stmt = conn
        .prepare(
            "SELECT id, parent_id, name, type, action, source_node_id, sort_order
             FROM consolidation_nodes WHERE consolidation_id = ?1",
        )
        .map_err(map_err)?;
    let cnodes: Vec<CNode> = stmt
        .query_map(params![cid], |r| {
            Ok(CNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                node_type: r.get(3)?,
                action: r.get(4)?,
                source_node_id: r.get(5)?,
                sort_order: r.get(6)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;

    if cnodes.is_empty() {
        out.push_str("_The consolidation tree is empty._\n");
        return Ok(out);
    }

    // Resolve source node origin paths (device + rel path).
    let mut src_paths: HashMap<i64, (String, String)> = HashMap::new();
    {
        let mut stmt = conn
            .prepare(
                "SELECT n.id, s.device_label, n.rel_path FROM nodes n
                 JOIN sources s ON s.id = n.source_id
                 WHERE n.id = ?1",
            )
            .map_err(map_err)?;
        for c in &cnodes {
            if let Some(sid) = c.source_node_id {
                if let Ok(row) = stmt.query_row(params![sid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                }) {
                    src_paths.insert(sid, row);
                }
            }
        }
    }

    // Build children index and compute target paths.
    let mut children: HashMap<Option<i64>, Vec<&CNode>> = HashMap::new();
    for c in &cnodes {
        children.entry(c.parent_id).or_default().push(c);
    }
    for v in children.values_mut() {
        v.sort_by_key(|c| c.sort_order);
    }

    // Depth-first walk producing both the tree rendering and the step list.
    let mut tree = String::new();
    let mut mkdirs: Vec<String> = Vec::new();
    let mut moves: Vec<String> = Vec::new();
    let mut copies: Vec<String> = Vec::new();
    let mut skips: Vec<String> = Vec::new();

    fn walk(
        parent: Option<i64>,
        prefix: &str,
        path: &str,
        children: &HashMap<Option<i64>, Vec<&CNode>>,
        src_paths: &HashMap<i64, (String, String)>,
        tree: &mut String,
        mkdirs: &mut Vec<String>,
        moves: &mut Vec<String>,
        copies: &mut Vec<String>,
        skips: &mut Vec<String>,
    ) {
        let Some(kids) = children.get(&parent) else {
            return;
        };
        for c in kids {
            let target = if path.is_empty() {
                c.name.clone()
            } else {
                format!("{path}/{}", c.name)
            };
            let marker = match c.action.as_str() {
                "skip" => " (skip)",
                "copy" => " (copy)",
                _ => "",
            };
            tree.push_str(&format!("{prefix}{}{marker}\n", c.name));

            let origin = c
                .source_node_id
                .and_then(|sid| src_paths.get(&sid))
                .map(|(dev, rel)| format!("[{dev}] {rel}"));
            match (c.node_type.as_str(), c.action.as_str(), origin) {
                ("directory", "skip", _) => skips.push(format!("{target}/ — leave as is")),
                ("directory", _, Some(o)) => moves.push(format!("Move folder {o} -> {target}/")),
                ("directory", _, None) => mkdirs.push(format!("Create folder: {target}/")),
                (_, "skip", Some(o)) => skips.push(format!("{o} — do not consolidate")),
                (_, "copy", Some(o)) => copies.push(format!("Copy {o} -> {target}")),
                (_, _, Some(o)) => moves.push(format!("Move {o} -> {target}")),
                _ => {}
            }
            if c.node_type == "directory" && c.action != "skip" {
                walk(
                    Some(c.id),
                    &format!("{prefix}  "),
                    &target,
                    children,
                    src_paths,
                    tree,
                    mkdirs,
                    moves,
                    copies,
                    skips,
                );
            }
        }
    }
    walk(
        None,
        "- ",
        "",
        &children,
        &src_paths,
        &mut tree,
        &mut mkdirs,
        &mut moves,
        &mut copies,
        &mut skips,
    );

    out.push_str("## Target tree\n\n");
    out.push_str(&tree);
    out.push('\n');

    if !mkdirs.is_empty() {
        out.push_str("## Step 1 — Create folders\n\n");
        for s in &mkdirs {
            out.push_str(&format!("- [ ] {s}\n"));
        }
        out.push('\n');
    }
    if !moves.is_empty() {
        out.push_str("## Step 2 — Move\n\n");
        for s in &moves {
            out.push_str(&format!("- [ ] {s}\n"));
        }
        out.push('\n');
    }
    if !copies.is_empty() {
        out.push_str("## Step 3 — Copy\n\n");
        for s in &copies {
            out.push_str(&format!("- [ ] {s}\n"));
        }
        out.push('\n');
    }
    // Virtual renames recorded in the path-limits step (files/folders inside
    // dragged-in source directories).
    let renames = pathfix::source_renames(&conn, workspace_id).map_err(map_err)?;
    if !renames.is_empty() {
        out.push_str("## Step 4 — Rename (path-length fixes)\n\n");
        let mut pstmt = conn
            .prepare(
                "SELECT s.device_label, n.rel_path FROM nodes n
                 JOIN sources s ON s.id = n.source_id WHERE n.id = ?1",
            )
            .map_err(map_err)?;
        for (id, orig, new) in &renames {
            let loc = pstmt
                .query_row(params![id], |r| {
                    Ok(format!(
                        "[{}] {}",
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?
                    ))
                })
                .unwrap_or_default();
            out.push_str(&format!("- [ ] Rename {loc} : '{orig}' -> '{new}'\n"));
        }
        out.push('\n');
    }

    if !skips.is_empty() {
        out.push_str("## Skipped (left in place)\n\n");
        for s in &skips {
            out.push_str(&format!("- {s}\n"));
        }
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Path-limit view
// ---------------------------------------------------------------------------

/// List over-limit paths in the consolidated end-state tree (step 3 of the
/// workflow: dedup → consolidate → fix path limits).
#[tauri::command]
pub fn pathfix_list(
    db: State<Db>,
    workspace_id: i64,
    limit: i64,
) -> CmdResult<Vec<PathLimitEntry>> {
    let conn = db.0.lock().unwrap();
    pathfix::list_over_limit(&conn, workspace_id, limit).map_err(map_err)
}

/// Rename one component of an over-limit path. `kind` is "cons" for
/// consolidation-tree nodes or "source" for nodes inside a dragged-in source
/// directory (stored as a virtual rename). Empty name reverts the edit.
#[tauri::command]
pub fn pathfix_rename(
    db: State<Db>,
    workspace_id: i64,
    kind: String,
    node_id: i64,
    new_name: String,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    pathfix::rename(&conn, workspace_id, &kind, node_id, &new_name).map_err(map_err)?;
    log_action(
        &conn,
        workspace_id,
        "pathfix_rename",
        &format!("Renamed {kind} node {node_id} to '{new_name}'"),
    );
    Ok(())
}

#[tauri::command]
pub fn pathfix_set_resolved(
    db: State<Db>,
    workspace_id: i64,
    kind: String,
    node_id: i64,
    resolved: bool,
) -> CmdResult<()> {
    let conn = db.0.lock().unwrap();
    pathfix::set_resolved(&conn, workspace_id, &kind, node_id, resolved).map_err(map_err)
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

/// Write text to a file chosen by the user (used to export the guide log).
#[tauri::command]
pub fn write_text_file(path: String, contents: String) -> CmdResult<()> {
    std::fs::write(&path, contents).map_err(map_err)
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
    let summary = crate::dbio::import_merge(&mut conn, Path::new(&path)).map_err(map_err)?;
    invalidate_scope_cache(&conn);
    let detail = format!(
        "Imported {} workspace(s), {} source(s), {} node(s) from '{path}'",
        summary.workspaces_added, summary.sources_added, summary.nodes_added
    );
    for ws_id in &summary.new_workspace_ids {
        log_action(&conn, *ws_id, "db_import", &detail);
    }
    Ok(summary)
}
