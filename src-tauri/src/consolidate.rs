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
) -> rusqlite::Result<Vec<ConsolidationNode>> {
    // 1. Fetch the whole subtree (root + every descendant) from `nodes` in one
    //    recursive query. Unlike pathfix.rs::load_source_subtrees (which starts
    //    from `parent_id = ?1` and only fetches children), the base case here
    //    is `id = ?1` so the root's own row comes back too.
    let snodes: Vec<SNode> = {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE sub(id) AS (
                 SELECT ?1
                 UNION ALL
                 SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id
             )
             SELECT n.id, n.parent_id, n.name, n.type, n.size, n.rel_path
             FROM nodes n WHERE n.id IN (SELECT id FROM sub)",
        )?;
        stmt.query_map(params![source_node_id], |r| {
            Ok(SNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                node_type: r.get(3)?,
                size: r.get(4)?,
                rel_path: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let Some(root) = snodes.iter().find(|n| n.id == source_node_id) else {
        return Ok(Vec::new()); // source_node_id no longer exists (e.g. device removed mid-drag)
    };

    // 2. Origin device label, looked up once (a whole dragged directory
    //    always belongs to one source) — avoids an N+1 join per row.
    let device_label: String = conn.query_row(
        "SELECT s.device_label FROM nodes n JOIN sources s ON s.id = n.source_id WHERE n.id = ?1",
        params![source_node_id],
        |r| r.get(0),
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
            "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;

        insert.execute(params![
            consolidation_id,
            parent_id,
            root.name,
            normalize_type(&root.node_type),
            root.id,
            root_sort_order,
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
        });

        let mut stack = vec![root.id];
        while let Some(src_pid) = stack.pop() {
            let new_pid = id_map[&src_pid];
            let Some(kids) = by_parent.get(&src_pid) else {
                continue;
            };
            for (i, k) in kids.iter().enumerate() {
                insert.execute(params![
                    consolidation_id,
                    new_pid,
                    k.name,
                    normalize_type(&k.node_type),
                    k.id,
                    (i as i64) + 1,
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
                });
                stack.push(k.id);
            }
        }
    } // `insert` (and its borrow of `tx`) dropped here, before commit

    tx.commit()?;
    Ok(out)
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

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id).unwrap();

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

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id).unwrap();

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

        let out = materialize_subtree(&mut conn, consolidation_id, None, root_id).unwrap();

        let root_row = out
            .iter()
            .find(|n| n.source_node_id == Some(root_id))
            .unwrap();
        assert_eq!(root_row.sort_order, 2);
    }
}
