//! Bulk-materializes a dragged-in source directory's entire subtree as real
//! `consolidation_nodes` rows, so every file and folder inside it becomes
//! individually addressable (movable/renamable/deletable), matching the
//! drag-and-drop UX already available for single files.

use crate::model::ConsolidationNode;
use rusqlite::{Connection, params};
use std::collections::HashMap;

struct SNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    size: i64,
    rel_path: String,
    is_alias: bool,
    /// `dup_annot.cross_dup`: the source's funnel hides this row.
    cross_dup: bool,
}

/// Mirrors the normalization already applied to single-file drags: only
/// "directory" is ever a directory, everything else (including symlinks)
/// becomes a plain "file" row.
fn normalize_type(t: &str) -> &'static str {
    if t == "directory" {
        "directory"
    } else {
        "file"
    }
}

pub fn materialize_subtree(
    conn: &mut Connection,
    consolidation_id: i64,
    parent_id: Option<i64>,
    source_node_id: i64,
    filter_cross_dup: bool,
) -> rusqlite::Result<Vec<ConsolidationNode>> {
    // 1. Fetch the whole subtree (root + every descendant) from `nodes` in one
    //    recursive query. Unlike pathfix.rs::load_source_subtrees (which starts
    //    from `parent_id = ?1` and only fetches children), the base case here
    //    is `id = ?1` so the root's own row comes back too.
    //
    //    The whole subtree always comes across, filtered or not. When
    //    `filter_cross_dup` is set (the source's "exclusive to this device"
    //    funnel is on for this drag), what the funnel hides is carried over
    //    *struck* rather than dropped: the consolidated tree is the blueprint
    //    for rebuilding the end state on disk, so "this was here, and it goes"
    //    is information the archivist needs, not noise to leave out. Each
    //    row's own `dup_annot.cross_dup` is read here; step 4 applies it
    //    level by level, as `get_tree` does.
    let snodes: Vec<SNode> = {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(id) AS (
                 SELECT ?1
                 UNION ALL
                 SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id
             )
             SELECT n.id, n.parent_id, n.name, n.type, n.size, n.rel_path,
                    n.alias_of IS NOT NULL, COALESCE(d.cross_dup, 0) != 0
             FROM nodes n
             LEFT JOIN dup_annot d ON d.node_id = n.id
             WHERE n.id IN (SELECT id FROM sub)",
        )?;
        stmt.query_map(params![source_node_id], |r| {
            Ok(SNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                node_type: r.get(3)?,
                size: r.get(4)?,
                rel_path: r.get(5)?,
                is_alias: r.get(6)?,
                cross_dup: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let Some(root) = snodes.iter().find(|n| n.id == source_node_id) else {
        return Ok(Vec::new()); // source_node_id no longer exists (e.g. device removed mid-drag)
    };

    // 2. Origin source id and device label, looked up once (a whole dragged
    //    directory always belongs to one source) — avoids an N+1 join per row.
    let (source_id, device_label): (i64, String) = conn.query_row(
        "SELECT s.id, s.device_label FROM nodes n JOIN sources s ON s.id = n.source_id WHERE n.id = ?1",
        params![source_node_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    // 3. Group descendants by source parent id; sibling order mirrors
    //    get_tree's convention (directories before files, then alphabetical)
    //    so the materialized subtree visually matches the source tree.
    let mut by_parent: HashMap<i64, Vec<&SNode>> = HashMap::new();
    for n in &snodes {
        if let Some(pid) = n.parent_id {
            by_parent.entry(pid).or_default().push(n);
        }
    }
    for kids in by_parent.values_mut() {
        kids.sort_by(|a, b| {
            (a.node_type == "file", a.name.to_lowercase())
                .cmp(&(b.node_type == "file", b.name.to_lowercase()))
        });
    }

    // 4. Insert root, then descendants in parent-before-child order (DFS via
    //    an explicit stack), tracking source id -> newly-inserted cons id so
    //    each descendant's parent_id resolves correctly.
    //
    //    Under the funnel a row is hidden when it or any ancestor below the
    //    root is `cross_dup`. `struck` is stored only where it is applied and
    //    inherited downward at read time, so only the *topmost* hidden row of
    //    each hidden run gets the flag; its descendants inherit it, and
    //    un-striking that one row brings the whole run back. The dragged root
    //    is never struck: the user could only drag a visible row.
    let root_sort_order: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM consolidation_nodes
         WHERE consolidation_id = ?1 AND parent_id IS ?2",
        params![consolidation_id, parent_id],
        |r| r.get(0),
    )?;

    let tx = conn.transaction()?;
    let mut out = Vec::with_capacity(snodes.len());
    let mut id_map: HashMap<i64, i64> = HashMap::new();

    {
        let mut insert = tx.prepare(
            "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order, struck)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;

        insert.execute(params![
            consolidation_id,
            parent_id,
            root.name,
            normalize_type(&root.node_type),
            root.id,
            root_sort_order,
            false,
        ])?;
        let root_new_id = tx.last_insert_rowid();
        id_map.insert(root.id, root_new_id);
        out.push(ConsolidationNode {
            id: root_new_id,
            consolidation_id,
            parent_id,
            name: root.name.clone(),
            node_type: normalize_type(&root.node_type).to_string(),
            source_node_id: Some(root.id),
            sort_order: root_sort_order,
            size: (root.node_type != "directory").then_some(root.size),
            origin_device: Some(device_label.clone()),
            origin_path: Some(root.rel_path.clone()),
            origin_source_id: Some(source_id),
            is_alias: root.is_alias,
            done: false,
            struck: false,
            original_name: None,
        });

        // (source id, whether the funnel hides it)
        let mut stack = vec![(root.id, false)];
        while let Some((src_pid, parent_hidden)) = stack.pop() {
            let new_pid = id_map[&src_pid];
            let Some(kids) = by_parent.get(&src_pid) else {
                continue;
            };
            for (i, k) in kids.iter().enumerate() {
                let own = filter_cross_dup && k.cross_dup;
                let struck = own && !parent_hidden;
                insert.execute(params![
                    consolidation_id,
                    new_pid,
                    k.name,
                    normalize_type(&k.node_type),
                    k.id,
                    (i as i64) + 1,
                    struck,
                ])?;
                let new_id = tx.last_insert_rowid();
                id_map.insert(k.id, new_id);
                out.push(ConsolidationNode {
                    id: new_id,
                    consolidation_id,
                    parent_id: Some(new_pid),
                    name: k.name.clone(),
                    node_type: normalize_type(&k.node_type).to_string(),
                    source_node_id: Some(k.id),
                    sort_order: (i as i64) + 1,
                    size: (k.node_type != "directory").then_some(k.size),
                    origin_device: Some(device_label.clone()),
                    origin_path: Some(k.rel_path.clone()),
                    origin_source_id: Some(source_id),
                    is_alias: k.is_alias,
                    done: false,
                    struck,
                    original_name: None,
                });
                stack.push((k.id, parent_hidden || own));
            }
        }
    } // `insert` (and its borrow of `tx`) dropped here, before commit

    tx.commit()?;
    Ok(out)
}

/// Removes a source's *files* from the consolidation tree, plus any virtual
/// renames that pointed at them.
///
/// Must run **before** the source row is deleted. `consolidation_nodes
/// .source_node_id` is `ON DELETE SET NULL`, so once `sources -> nodes`
/// cascades there is nothing left to identify which consolidation rows came
/// from this source; they would survive as ghosts that still count as files
/// while reporting no size and no origin.
///
/// Folders are deliberately kept. Deciding whether the user has since
/// cross-populated one with files from another source costs far more than a
/// stray empty folder is worth, and they can delete it by hand. `type` here is
/// only ever 'file' or 'directory' (see [`normalize_type`]), so the filter is
/// exact.
pub fn purge_source_files(conn: &Connection, source_id: i64) -> rusqlite::Result<()> {
    // Virtual renames first: the `kind='cons'` ones are keyed on the very
    // consolidation rows the last statement removes.
    conn.execute(
        "DELETE FROM pathfix_state
          WHERE kind = 'cons'
            AND ref_id IN (SELECT cn.id
                             FROM consolidation_nodes cn
                            WHERE cn.type = 'file'
                              AND cn.source_node_id IN
                                  (SELECT id FROM nodes WHERE source_id = ?1))",
        params![source_id],
    )?;
    conn.execute(
        "DELETE FROM pathfix_state
          WHERE kind = 'source'
            AND ref_id IN (SELECT id FROM nodes WHERE source_id = ?1)",
        params![source_id],
    )?;
    conn.execute(
        "DELETE FROM consolidation_nodes
          WHERE type = 'file'
            AND source_node_id IN (SELECT id FROM nodes WHERE source_id = ?1)",
        params![source_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, parse};

    /// Build an in-memory workspace with one source and a consolidation row,
    /// and return (conn, workspace_id, source_id, consolidation_id).
    fn setup(json: &str) -> (Connection, i64, i64, i64) {
        let mut conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        let flat = parse::parse_tree_json(json).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'json', 'disc-a', 'disc-a', 't', ?2, ?3)",
            params![ws, flat.total_size, flat.file_count],
        )
        .unwrap();
        let source_id = tx.last_insert_rowid();
        parse::insert_nodes(&tx, source_id, &flat).unwrap();
        tx.commit().unwrap();

        conn.execute(
            "INSERT INTO consolidations (workspace_id, name) VALUES (?1, 'Consolidated')",
            params![ws],
        )
        .unwrap();
        let consolidation_id = conn.last_insert_rowid();

        (conn, ws, source_id, consolidation_id)
    }

    fn node_id(conn: &Connection, source_id: i64, rel_path: &str) -> i64 {
        conn.query_row(
            "SELECT id FROM nodes WHERE source_id = ?1 AND rel_path = ?2",
            params![source_id, rel_path],
            |r| r.get(0),
        )
        .unwrap()
    }

    const NESTED_TREE: &str = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
        {"type":"directory","name":"photos","inode":1,"dev":10,"contents":[
            {"type":"directory","name":"2024","inode":2,"dev":10,"contents":[
                {"type":"file","name":"a.jpg","inode":3,"dev":10,"size":500000,"time":"2024-01-01_10:00:00"},
                {"type":"file","name":"b.jpg","inode":4,"dev":10,"size":800000,"time":"2024-01-01_10:05:00"}
            ]}
        ]}
    ]}]"#;

    #[test]
    fn materializes_full_subtree_with_correct_parent_chain() {
        let (mut conn, _ws, source_id, consolidation_id) = setup(NESTED_TREE);
        let root_id = node_id(&conn, source_id, "photos");

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id, false).unwrap();

        // photos, 2024, a.jpg, b.jpg = 4 nodes total.
        assert_eq!(out.len(), 4);

        let root_row = out
            .iter()
            .find(|n| n.source_node_id == Some(root_id))
            .unwrap();
        assert_eq!(
            root_row.parent_id, None,
            "root's parent_id should be the argument passed in"
        );
        assert_eq!(root_row.name, "photos");

        let year_src_id = node_id(&conn, source_id, "photos/2024");
        let year_row = out
            .iter()
            .find(|n| n.source_node_id == Some(year_src_id))
            .unwrap();
        assert_eq!(year_row.parent_id, Some(root_row.id));

        let a_src_id = node_id(&conn, source_id, "photos/2024/a.jpg");
        let a_row = out
            .iter()
            .find(|n| n.source_node_id == Some(a_src_id))
            .unwrap();
        assert_eq!(a_row.parent_id, Some(year_row.id));
        assert_eq!(a_row.size, Some(500000));
    }

    #[test]
    fn materialize_normalizes_link_type_to_file() {
        let json = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
            {"type":"directory","name":"stuff","inode":1,"dev":10,"contents":[
                {"type":"link","name":"shortcut","inode":2,"dev":10,"size":0,"time":"2024-01-01_10:00:00"}
            ]}
        ]}]"#;
        let (mut conn, _ws, source_id, consolidation_id) = setup(json);
        let root_id = node_id(&conn, source_id, "stuff");

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id, false).unwrap();

        let link_src_id = node_id(&conn, source_id, "stuff/shortcut");
        let link_row = out
            .iter()
            .find(|n| n.source_node_id == Some(link_src_id))
            .unwrap();
        assert_eq!(link_row.node_type, "file");
    }

    #[test]
    fn materialize_assigns_root_sort_order_after_existing_siblings() {
        let (mut conn, _ws, source_id, consolidation_id) = setup(NESTED_TREE);
        conn.execute(
            "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order)
             VALUES (?1, NULL, 'existing', 'directory', NULL, 1)",
            params![consolidation_id],
        )
        .unwrap();
        let root_id = node_id(&conn, source_id, "photos");

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id, false).unwrap();

        let root_row = out
            .iter()
            .find(|n| n.source_node_id == Some(root_id))
            .unwrap();
        assert_eq!(root_row.sort_order, 2);
    }

    /// Mark `rel_path`'s node as a cross-device duplicate in `dup_annot`, the
    /// same flag the "exclusive to this device" funnel filters on. All
    /// `dup_annot` columns have defaults, so setting just `cross_dup` is valid.
    fn mark_cross_dup(conn: &Connection, source_id: i64, rel_path: &str) {
        let id = node_id(conn, source_id, rel_path);
        conn.execute(
            "INSERT INTO dup_annot (node_id, cross_dup) VALUES (?1, 1)",
            params![id],
        )
        .unwrap();
    }

    /// The row's own stored `struck` flag, read back from the database so a
    /// test checks what was persisted, not just what was returned.
    fn stored_struck(conn: &Connection, cons_id: i64) -> bool {
        conn.query_row(
            "SELECT struck FROM consolidation_nodes WHERE id = ?1",
            params![cons_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The returned row for source node `src`, asserting its returned
    /// `struck` agrees with the stored one.
    fn row_for(conn: &Connection, out: &[ConsolidationNode], src: i64) -> ConsolidationNode {
        let row = out
            .iter()
            .find(|n| n.source_node_id == Some(src))
            .expect("every source row is materialized")
            .clone();
        assert_eq!(row.struck, stored_struck(conn, row.id));
        row
    }

    #[test]
    fn materialize_strikes_an_alias_whose_canonical_is_cross_dup() {
        // Dragging a filtered directory once carried across a second *name*
        // for content whose canonical the same filter had just hidden.
        // `rebuild_annotations` gives an alias its canonical's `cross_dup`, so
        // both are hidden by the funnel -- and both must now come across
        // struck, together. A dragged alias is still flagged `is_alias` so its
        // bytes are not double-counted in the pane's rollup.
        let (mut conn, _ws, source_id, consolidation_id) = setup(NESTED_TREE);
        let canonical = node_id(&conn, source_id, "photos/2024/b.jpg");
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, alias_of)
             SELECT source_id, parent_id, 'b_alias.jpg', 'photos/2024/b_alias.jpg', 'file',
                    size, ?1
             FROM nodes WHERE id = ?1",
            params![canonical],
        )
        .unwrap();
        let alias: i64 = conn
            .query_row(
                "SELECT id FROM nodes WHERE rel_path = 'photos/2024/b_alias.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Unfiltered, both names come across, and the alias is marked.
        let root_id = node_id(&conn, source_id, "photos");
        let all = materialize_subtree(&mut conn, consolidation_id, None, root_id, false).unwrap();
        let alias_row = all
            .iter()
            .find(|n| n.source_node_id == Some(alias))
            .expect("alias materializes when unfiltered -- it is a real name");
        assert!(alias_row.is_alias, "flagged so size rollups skip its bytes");
        assert!(
            !all.iter()
                .find(|n| n.source_node_id == Some(canonical))
                .unwrap()
                .is_alias
        );

        // Now hide the canonical, as rebuild_annotations does for both.
        mark_cross_dup(&conn, source_id, "photos/2024/b.jpg");
        conn.execute(
            "INSERT INTO dup_annot (node_id, cross_dup) VALUES (?1, 1)",
            params![alias],
        )
        .unwrap();
        let filtered =
            materialize_subtree(&mut conn, consolidation_id, None, root_id, true).unwrap();
        let alias_row = row_for(&conn, &filtered, alias);
        assert!(alias_row.struck, "an alias of a hidden canonical comes across struck");
        assert!(alias_row.is_alias);
        assert!(row_for(&conn, &filtered, canonical).struck);
    }

    #[test]
    fn materialize_strikes_cross_dup_descendants_when_filtering() {
        // A filtered drag used to drop what the funnel hid, so the
        // consolidated tree -- the blueprint for the end state on disk --
        // lost every item the archivist had to delete. It now carries the
        // whole subtree and strikes the hidden rows.
        let (mut conn, _ws, source_id, consolidation_id) = setup(NESTED_TREE);
        mark_cross_dup(&conn, source_id, "photos/2024/b.jpg");
        let root_id = node_id(&conn, source_id, "photos");
        let year = node_id(&conn, source_id, "photos/2024");
        let a = node_id(&conn, source_id, "photos/2024/a.jpg");
        let b = node_id(&conn, source_id, "photos/2024/b.jpg");

        let filtered =
            materialize_subtree(&mut conn, consolidation_id, None, root_id, true).unwrap();

        assert_eq!(filtered.len(), 4, "hidden rows come across, not dropped");
        assert!(row_for(&conn, &filtered, b).struck);
        for live in [root_id, year, a] {
            assert!(!row_for(&conn, &filtered, live).struck);
        }

        // Without the filter nothing is struck: the funnel was off, so the
        // user saw (and chose) everything.
        let unfiltered =
            materialize_subtree(&mut conn, consolidation_id, None, root_id, false).unwrap();
        assert_eq!(unfiltered.len(), 4);
        for n in &unfiltered {
            assert!(!n.struck);
            assert!(!stored_struck(&conn, n.id));
        }
    }

    #[test]
    fn materialize_strikes_only_the_topmost_row_of_a_hidden_subdir() {
        let (mut conn, _ws, source_id, consolidation_id) = setup(NESTED_TREE);
        // Every leaf under photos/2024 is cross_dup, so the funnel hides the
        // whole "2024" dir. rollup.rs annotates the directory row itself in
        // that case (cross_dup when *every* leaf beneath it is), so mark it
        // here too -- the drag keys off each row's own annotation, not a
        // recomputed rollup.
        mark_cross_dup(&conn, source_id, "photos/2024/a.jpg");
        mark_cross_dup(&conn, source_id, "photos/2024/b.jpg");
        mark_cross_dup(&conn, source_id, "photos/2024");
        let root_id = node_id(&conn, source_id, "photos");
        let year = node_id(&conn, source_id, "photos/2024");

        let filtered =
            materialize_subtree(&mut conn, consolidation_id, None, root_id, true).unwrap();

        assert_eq!(filtered.len(), 4);
        assert!(!row_for(&conn, &filtered, root_id).struck);
        assert!(row_for(&conn, &filtered, year).struck);
        // `struck` is inherited at read time, so the files beneath carry no
        // flag of their own -- un-striking "2024" must bring them back.
        for leaf in ["photos/2024/a.jpg", "photos/2024/b.jpg"] {
            let id = node_id(&conn, source_id, leaf);
            assert!(!row_for(&conn, &filtered, id).struck, "{leaf} inherits");
        }
    }

    /// Adds a second source to an existing workspace, so a deletion test can
    /// prove it removes one source's rows *and nothing else*.
    fn add_source(conn: &mut Connection, ws: i64, label: &str, json: &str) -> i64 {
        let flat = parse::parse_tree_json(json).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'json', ?2, ?2, 't', ?3, ?4)",
            params![ws, label, flat.total_size, flat.file_count],
        )
        .unwrap();
        let source_id = tx.last_insert_rowid();
        parse::insert_nodes(&tx, source_id, &flat).unwrap();
        tx.commit().unwrap();
        source_id
    }

    #[test]
    fn purging_a_source_drops_its_files_but_keeps_its_folders_and_other_sources() {
        // Deleting a source used to leave its consolidation rows behind: the
        // `source_node_id` FK is ON DELETE SET NULL, so the cascade erased the
        // only link rather than the rows. They survived as ghosts that still
        // counted as files while reporting no size and no origin.
        let (mut conn, ws, source_a, consolidation_id) = setup(NESTED_TREE);
        let source_b = add_source(&mut conn, ws, "disc-b", NESTED_TREE);

        let a_root = node_id(&conn, source_a, "photos");
        let b_root = node_id(&conn, source_b, "photos");
        materialize_subtree(&mut conn, consolidation_id, None, a_root, false).unwrap();
        materialize_subtree(&mut conn, consolidation_id, None, b_root, false).unwrap();

        // A virtual rename hanging off one of A's dragged-in files, and one off
        // a hand-made folder that must survive.
        let a_file = node_id(&conn, source_a, "photos/2024/a.jpg");
        let a_file_cons: i64 = conn
            .query_row(
                "SELECT id FROM consolidation_nodes WHERE source_node_id = ?1",
                params![a_file],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO pathfix_state (workspace_id, kind, ref_id, new_name)
             VALUES (?1, 'cons', ?2, 'renamed.jpg'), (?1, 'source', ?3, 'other.jpg')",
            params![ws, a_file_cons, a_file],
        )
        .unwrap();

        let files_from = |conn: &Connection, sid: i64| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM consolidation_nodes cn
                   JOIN nodes n ON n.id = cn.source_node_id
                  WHERE cn.type = 'file' AND n.source_id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .unwrap()
        };
        let dirs_from = |conn: &Connection, sid: i64| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM consolidation_nodes cn
                   JOIN nodes n ON n.id = cn.source_node_id
                  WHERE cn.type = 'directory' AND n.source_id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .unwrap()
        };

        // Both sources start out fully represented, so the assertions below
        // cannot pass vacuously.
        assert!(files_from(&conn, source_a) > 0);
        assert!(dirs_from(&conn, source_a) > 0);
        let b_files_before = files_from(&conn, source_b);
        let b_dirs_before = dirs_from(&conn, source_b);
        assert!(b_files_before > 0);

        purge_source_files(&conn, source_a).unwrap();

        assert_eq!(files_from(&conn, source_a), 0, "A's files must be gone");
        assert!(
            dirs_from(&conn, source_a) > 0,
            "A's folders are deliberately left behind"
        );
        assert_eq!(files_from(&conn, source_b), b_files_before, "B untouched");
        assert_eq!(dirs_from(&conn, source_b), b_dirs_before, "B untouched");

        // The virtual rename on A's file goes with it; nothing else does.
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM pathfix_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "both dangling renames swept");
    }
}
