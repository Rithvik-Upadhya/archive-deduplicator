//! Windows path-length guidance for the **consolidated end-state tree**.
//!
//! The workflow is: (1) deduplicate, (2) consolidate into a target tree,
//! (3) fix path-length problems in that final tree. So this module walks the
//! consolidation tree — not the raw sources. Every dragged-in source
//! directory is fully materialized into real `consolidation_nodes` rows at
//! drag time (see `consolidate.rs`), so this walk never needs to reach into
//! `nodes` itself.
//!
//! `build_tree` returns every node in the consolidation tree, annotated with
//! `over_limit` (propagated to every ancestor of an offending leaf) and
//! `path_length`, so the frontend can render the whole tree with live
//! red/clear feedback. Renames are applied directly to
//! `consolidation_nodes.name` (the end-state tree is the user's own plan);
//! the true original name is recorded once in `pathfix_state` so it can be
//! reverted.

use crate::model::PathTreeNode;
use rusqlite::{Connection, params};
use std::collections::HashMap;

/// A consolidation-tree node.
struct CNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    sort_order: i64,
    /// Where this node came from, for the tree's hover text. `None` for a
    /// folder the user made by hand, which has no source.
    origin_device: Option<String>,
    origin_path: Option<String>,
    origin_source_id: Option<i64>,
}

/// Stored rename edit for a consolidation node.
#[derive(Clone)]
struct FixState {
    new_name: String,
    original_name: String,
}

fn load_cnodes(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<CNode>> {
    // The two joins are the same pair `consolidation_get` uses, and they must
    // stay LEFT: a folder the user created by hand has `source_node_id IS NULL`,
    // and an inner join would drop it from the Fix Paths tree entirely -- a far
    // worse bug than a missing tooltip.
    let mut stmt = conn.prepare(
        "SELECT cn.id, cn.parent_id, cn.name, cn.type, cn.sort_order,
                s.device_label, n.rel_path, s.id
         FROM consolidation_nodes cn
         JOIN consolidations c ON c.id = cn.consolidation_id
         LEFT JOIN nodes n ON n.id = cn.source_node_id
         LEFT JOIN sources s ON s.id = n.source_id
         WHERE c.workspace_id = ?1",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(CNode {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            name: r.get(2)?,
            node_type: r.get(3)?,
            sort_order: r.get(4)?,
            origin_device: r.get(5)?,
            origin_path: r.get(6)?,
            origin_source_id: r.get(7)?,
        })
    })?;
    rows.collect()
}

fn load_state(conn: &Connection, workspace_id: i64) -> rusqlite::Result<HashMap<i64, FixState>> {
    let mut stmt = conn.prepare(
        "SELECT ref_id, new_name, original_name FROM pathfix_state
         WHERE workspace_id = ?1 AND kind = 'cons'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (ref_id, new_name, original_name) = row?;
        if new_name.is_empty() {
            continue; // no active edit (includes now-inert legacy resolved-only rows)
        }
        map.insert(
            ref_id,
            FixState {
                new_name,
                original_name,
            },
        );
    }
    Ok(map)
}

struct Walker<'a> {
    children: &'a HashMap<Option<i64>, Vec<usize>>,
    cnodes: &'a [CNode],
    state: &'a HashMap<i64, FixState>,
    limit: i64,
    out: Vec<PathTreeNode>,
    stack: Vec<usize>,
}

impl<'a> Walker<'a> {
    fn effective(&self, id: i64, original: &str) -> (String, String, bool) {
        match self.state.get(&id) {
            Some(s) => (s.new_name.clone(), s.original_name.clone(), true),
            None => (original.to_string(), original.to_string(), false),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_node(
        &mut self,
        parent_id: Option<i64>,
        id: i64,
        name: String,
        original_name: String,
        node_type: &str,
        edited: bool,
        path_length: i64,
        sort_order: i64,
        origin_device: Option<String>,
        origin_path: Option<String>,
        origin_source_id: Option<i64>,
    ) {
        let idx = self.out.len();
        self.out.push(PathTreeNode {
            id,
            parent_id,
            name,
            original_name,
            node_type: node_type.to_string(),
            edited,
            over_limit: false,
            path_length,
            sort_order,
            origin_device,
            origin_path,
            origin_source_id,
        });
        self.stack.push(idx);
    }

    fn pop_node(&mut self) {
        self.stack.pop();
    }

    /// If `path_length` exceeds `limit`, mark this leaf and every ancestor
    /// currently on the stack as over_limit.
    fn mark_if_over(&mut self, path_length: i64) {
        if path_length > self.limit {
            for &idx in &self.stack {
                self.out[idx].over_limit = true;
            }
        }
    }

    /// Walk the consolidation tree. `sort_order` (applied once, in
    /// `build_tree`, to the `children` map) is emission order, which is also
    /// the tree's display order on the frontend.
    fn walk_cons(&mut self, parent: Option<i64>, prefix: &str) {
        let Some(list) = self.children.get(&parent).cloned() else {
            return;
        };
        for i in list {
            let c = &self.cnodes[i];
            let (name, original_name, edited) = self.effective(c.id, &c.name);
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}\\{name}")
            };
            let path_length = path.chars().count() as i64;
            let has_kids = self.children.contains_key(&Some(c.id));
            let node_type = c.node_type.clone();
            self.push_node(
                parent,
                c.id,
                name,
                original_name,
                &node_type,
                edited,
                path_length,
                c.sort_order,
                c.origin_device.clone(),
                c.origin_path.clone(),
                c.origin_source_id,
            );

            if node_type == "directory" && has_kids {
                self.walk_cons(Some(c.id), &path);
            } else {
                self.mark_if_over(path_length);
            }
            self.pop_node();
        }
    }
}

/// Build the consolidated end-state tree annotated with path-length
/// guidance. Every node is returned, not just over-limit ones; `over_limit`
/// marks every node on an offending root-to-leaf chain.
pub fn build_tree(
    conn: &Connection,
    workspace_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<PathTreeNode>> {
    let cnodes = load_cnodes(conn, workspace_id)?;
    if cnodes.is_empty() {
        return Ok(Vec::new());
    }
    let state = load_state(conn, workspace_id)?;

    let mut children: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
    for (i, c) in cnodes.iter().enumerate() {
        children.entry(c.parent_id).or_default().push(i);
    }
    for v in children.values_mut() {
        v.sort_by_key(|&i| cnodes[i].sort_order);
    }

    let mut walker = Walker {
        children: &children,
        cnodes: &cnodes,
        state: &state,
        limit,
        out: Vec::new(),
        stack: Vec::new(),
    };
    walker.walk_cons(None, "");
    Ok(walker.out)
}

/// Rename a node in the consolidated end-state tree, recording the original
/// name once in `pathfix_state`. Passing an empty name reverts the edit.
pub fn rename(
    conn: &Connection,
    workspace_id: i64,
    ref_id: i64,
    new_name: &str,
) -> rusqlite::Result<()> {
    let current: String = conn.query_row(
        "SELECT name FROM consolidation_nodes WHERE id = ?1",
        params![ref_id],
        |r| r.get(0),
    )?;
    if new_name.is_empty() {
        // Revert to original if we have one recorded.
        let orig: Option<String> = conn
            .query_row(
                "SELECT original_name FROM pathfix_state
                 WHERE workspace_id = ?1 AND kind = 'cons' AND ref_id = ?2",
                params![workspace_id, ref_id],
                |r| r.get(0),
            )
            .ok();
        if let Some(orig) = orig.filter(|o| !o.is_empty()) {
            conn.execute(
                "UPDATE consolidation_nodes SET name = ?1 WHERE id = ?2",
                params![orig, ref_id],
            )?;
        }
        conn.execute(
            "DELETE FROM pathfix_state
             WHERE workspace_id = ?1 AND kind = 'cons' AND ref_id = ?2",
            params![workspace_id, ref_id],
        )?;
    } else {
        conn.execute(
            "INSERT INTO pathfix_state (workspace_id, kind, ref_id, new_name, original_name)
             VALUES (?1, 'cons', ?2, ?3, ?4)
             ON CONFLICT(workspace_id, kind, ref_id)
             DO UPDATE SET new_name = excluded.new_name",
            params![workspace_id, ref_id, new_name, current],
        )?;
        conn.execute(
            "UPDATE consolidation_nodes SET name = ?1 WHERE id = ?2",
            params![new_name, ref_id],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Build an in-memory workspace with an empty consolidation.
    /// Returns (conn, workspace_id, consolidation_id).
    fn setup() -> (Connection, i64, i64) {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO consolidations (workspace_id, name) VALUES (?1, 'Consolidated')",
            params![ws],
        )
        .unwrap();
        let consolidation_id = conn.last_insert_rowid();

        (conn, ws, consolidation_id)
    }

    fn insert_cnode(
        conn: &Connection,
        consolidation_id: i64,
        parent_id: Option<i64>,
        name: &str,
        node_type: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, sort_order)
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![consolidation_id, parent_id, name, node_type],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn healthy_tree_has_no_over_limit_nodes() {
        let (conn, ws, cid) = setup();
        let dir = insert_cnode(&conn, cid, None, "docs", "directory");
        insert_cnode(&conn, cid, Some(dir), "readme.txt", "file");

        let nodes = build_tree(&conn, ws, 260).unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().all(|n| !n.over_limit));
    }

    #[test]
    fn over_limit_leaf_marks_ancestors_but_not_siblings() {
        let (conn, ws, cid) = setup();
        let root = insert_cnode(&conn, cid, None, "root", "directory");
        insert_cnode(&conn, cid, Some(root), "a.txt", "file");
        let bigfolder = insert_cnode(&conn, cid, Some(root), "bigfolder", "directory");
        insert_cnode(&conn, cid, Some(bigfolder), "f.txt", "file");

        // Limit small enough that only the bigfolder/f.txt branch is over.
        let limit = "root\\bigfolder\\f.txt".chars().count() as i64 - 1;
        let nodes = build_tree(&conn, ws, limit).unwrap();

        let by_name: HashMap<&str, &PathTreeNode> =
            nodes.iter().map(|n| (n.name.as_str(), n)).collect();
        assert!(!by_name["a.txt"].over_limit, "short sibling stays clear");
        assert!(by_name["f.txt"].over_limit, "offending leaf is marked");
        assert!(by_name["bigfolder"].over_limit, "ancestor is marked");
        assert!(by_name["root"].over_limit, "root ancestor is marked");
    }

    #[test]
    fn nested_materialized_tree_reports_each_leaf_once() {
        let (conn, ws, cid) = setup();
        let photos = insert_cnode(&conn, cid, None, "photos", "directory");
        let year = insert_cnode(&conn, cid, Some(photos), "2024", "directory");
        insert_cnode(&conn, cid, Some(year), "a.jpg", "file");
        insert_cnode(&conn, cid, Some(year), "b.jpg", "file");

        let nodes = build_tree(&conn, ws, 260).unwrap();
        assert_eq!(nodes.len(), 4);
        let a_count = nodes.iter().filter(|n| n.name == "a.jpg").count();
        let b_count = nodes.iter().filter(|n| n.name == "b.jpg").count();
        assert_eq!(a_count, 1);
        assert_eq!(b_count, 1);

        let by_name: HashMap<&str, &PathTreeNode> =
            nodes.iter().map(|n| (n.name.as_str(), n)).collect();
        assert_eq!(by_name["a.jpg"].parent_id, Some(year));
        assert_eq!(by_name["2024"].parent_id, Some(photos));
    }
}
