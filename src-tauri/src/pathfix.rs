//! Windows path-length guidance for the **consolidated end-state tree**.
//!
//! The workflow is: (1) deduplicate, (2) consolidate into a target tree,
//! (3) fix path-length problems in that final tree. So this module walks the
//! consolidation tree — not the raw sources. When a consolidation node was
//! dragged in from a source *directory*, the whole source subtree beneath it
//! comes along in the end state, so we descend into `nodes` as well.
//!
//! Renames of consolidation nodes are applied directly to
//! `consolidation_nodes.name` (the end-state tree is the user's own plan);
//! renames of files/folders *inside* a dragged source directory are stored as
//! virtual edits in `pathfix_state` and folded into path computation and the
//! export guide.

use crate::model::{PathComponent, PathLimitEntry};
use rusqlite::{params, Connection};
use std::collections::HashMap;

/// A consolidation-tree node.
struct CNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    action: String,
    source_node_id: Option<i64>,
    sort_order: i64,
}

/// A source-tree node (only loaded for subtrees referenced by the consolidation).
struct SNode {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
}

/// Stored virtual edit / resolved flag.
#[derive(Default, Clone)]
struct FixState {
    new_name: Option<String>,
    resolved: bool,
}

fn load_cnodes(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<CNode>> {
    let mut stmt = conn.prepare(
        "SELECT cn.id, cn.parent_id, cn.name, cn.type, cn.action, cn.source_node_id, cn.sort_order
         FROM consolidation_nodes cn
         JOIN consolidations c ON c.id = cn.consolidation_id
         WHERE c.workspace_id = ?1",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(CNode {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            name: r.get(2)?,
            node_type: r.get(3)?,
            action: r.get(4)?,
            source_node_id: r.get(5)?,
            sort_order: r.get(6)?,
        })
    })?;
    rows.collect()
}

/// Load the full subtree of `nodes` under each of the given source roots.
fn load_source_subtrees(
    conn: &Connection,
    roots: &[i64],
) -> rusqlite::Result<HashMap<i64, Vec<SNode>>> {
    let mut out: HashMap<i64, Vec<SNode>> = HashMap::new();
    let mut stmt = conn.prepare(
        "WITH RECURSIVE sub(id) AS (
             SELECT id FROM nodes WHERE parent_id = ?1
             UNION ALL
             SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id
         )
         SELECT n.id, n.parent_id, n.name, n.type FROM nodes n
         WHERE n.id IN (SELECT id FROM sub)",
    )?;
    for &root in roots {
        let rows = stmt.query_map(params![root], |r| {
            Ok(SNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                node_type: r.get(3)?,
            })
        })?;
        let nodes: Vec<SNode> = rows.collect::<rusqlite::Result<_>>()?;
        out.insert(root, nodes);
    }
    Ok(out)
}

fn load_state(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<(String, i64), FixState>> {
    let mut stmt = conn.prepare(
        "SELECT kind, ref_id, new_name, resolved FROM pathfix_state WHERE workspace_id = ?1",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, i64>(1)?),
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)? != 0,
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (key, new_name, resolved) = row?;
        map.insert(
            key,
            FixState {
                new_name: if new_name.is_empty() {
                    None
                } else {
                    Some(new_name)
                },
                resolved,
            },
        );
    }
    Ok(map)
}

struct Walker<'a> {
    children: &'a HashMap<Option<i64>, Vec<usize>>,
    cnodes: &'a [CNode],
    subtrees: &'a HashMap<i64, Vec<SNode>>,
    state: &'a HashMap<(String, i64), FixState>,
    limit: i64,
    out: Vec<PathLimitEntry>,
}

impl<'a> Walker<'a> {
    fn effective(&self, kind: &str, id: i64, original: &str) -> (String, bool) {
        match self
            .state
            .get(&(kind.to_string(), id))
            .and_then(|s| s.new_name.clone())
        {
            Some(n) => (n, true),
            None => (original.to_string(), false),
        }
    }

    fn emit_if_over(
        &mut self,
        path: &str,
        components: &[PathComponent],
        leaf_kind: &str,
        leaf_id: i64,
    ) {
        let len = path.chars().count() as i64;
        if len > self.limit {
            let resolved = self
                .state
                .get(&(leaf_kind.to_string(), leaf_id))
                .map(|s| s.resolved)
                .unwrap_or(false);
            self.out.push(PathLimitEntry {
                node_id: leaf_id,
                leaf_kind: leaf_kind.into(),
                effective_path: path.to_string(),
                length: len,
                resolved,
                components: components.to_vec(),
            });
        }
    }

    /// Walk a dragged-in source subtree beneath `prefix`.
    fn walk_source_children(
        &mut self,
        kids: &HashMap<Option<i64>, Vec<usize>>,
        nodes: &[SNode],
        parent: i64,
        prefix: &str,
        components: &mut Vec<PathComponent>,
    ) {
        let Some(list) = kids.get(&Some(parent)) else {
            return;
        };
        for &i in list {
            let n = &nodes[i];
            let (name, edited) = self.effective("source", n.id, &n.name);
            let path = format!("{prefix}\\{name}");
            components.push(PathComponent {
                node_id: n.id,
                kind: "source".into(),
                name: name.clone(),
                original_name: n.name.clone(),
                node_type: n.node_type.clone(),
                edited,
            });
            let has_kids = kids.contains_key(&Some(n.id));
            if n.node_type == "directory" && has_kids {
                self.walk_source_children(kids, nodes, n.id, &path, components);
            } else {
                self.emit_if_over(&path, components, "source", n.id);
            }
            components.pop();
        }
    }

    fn walk_source(&mut self, root: i64, prefix: &str, components: &mut Vec<PathComponent>) {
        let Some(nodes) = self.subtrees.get(&root) else {
            return;
        };
        let mut kids: HashMap<Option<i64>, Vec<usize>> = HashMap::new();
        for (i, n) in nodes.iter().enumerate() {
            kids.entry(n.parent_id).or_default().push(i);
        }
        // Children of the dragged root have parent_id == root.
        self.walk_source_children(&kids, nodes, root, prefix, components);
    }

    /// Walk the consolidation tree itself.
    fn walk_cons(
        &mut self,
        parent: Option<i64>,
        prefix: &str,
        components: &mut Vec<PathComponent>,
    ) {
        let Some(list) = self.children.get(&parent).cloned() else {
            return;
        };
        for i in list {
            let c = &self.cnodes[i];
            if c.action == "skip" {
                continue;
            }
            let (name, edited) = self.effective("cons", c.id, &c.name);
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}\\{name}")
            };
            components.push(PathComponent {
                node_id: c.id,
                kind: "cons".into(),
                name: name.clone(),
                original_name: c.name.clone(),
                node_type: c.node_type.clone(),
                edited,
            });

            let has_cons_kids = self.children.contains_key(&Some(c.id));
            let src_subtree = if c.node_type == "directory" {
                c.source_node_id.filter(|sid| {
                    self.subtrees
                        .get(sid)
                        .map(|v| !v.is_empty())
                        .unwrap_or(false)
                })
            } else {
                None
            };

            if c.node_type == "directory" && (has_cons_kids || src_subtree.is_some()) {
                if has_cons_kids {
                    self.walk_cons(Some(c.id), &path, components);
                }
                if let Some(sid) = src_subtree {
                    self.walk_source(sid, &path, components);
                }
            } else {
                self.emit_if_over(&path, components, "cons", c.id);
            }
            components.pop();
        }
    }
}

/// List leaf paths in the consolidated end-state tree whose effective length
/// exceeds `limit`.
pub fn list_over_limit(
    conn: &Connection,
    workspace_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<PathLimitEntry>> {
    let cnodes = load_cnodes(conn, workspace_id)?;
    if cnodes.is_empty() {
        return Ok(Vec::new());
    }
    let state = load_state(conn, workspace_id)?;

    // Source subtrees referenced by directory consolidation nodes.
    let dir_roots: Vec<i64> = cnodes
        .iter()
        .filter(|c| c.node_type == "directory" && c.action != "skip")
        .filter_map(|c| c.source_node_id)
        .collect();
    let subtrees = load_source_subtrees(conn, &dir_roots)?;

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
        subtrees: &subtrees,
        state: &state,
        limit,
        out: Vec::new(),
    };
    let mut components: Vec<PathComponent> = Vec::new();
    walker.walk_cons(None, "", &mut components);

    let mut out = walker.out;
    out.sort_by(|a, b| b.length.cmp(&a.length));
    Ok(out)
}

/// Rename a path component. For `cons` nodes the consolidation tree itself is
/// updated (recording the original name once); for `source` nodes a virtual
/// rename edit is stored. Passing an empty name reverts the edit.
pub fn rename(
    conn: &Connection,
    workspace_id: i64,
    kind: &str,
    ref_id: i64,
    new_name: &str,
) -> rusqlite::Result<()> {
    match kind {
        "cons" => {
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
        }
        _ => {
            // Virtual rename of a node inside a dragged source directory.
            if new_name.is_empty() {
                conn.execute(
                    "DELETE FROM pathfix_state
                     WHERE workspace_id = ?1 AND kind = 'source' AND ref_id = ?2",
                    params![workspace_id, ref_id],
                )?;
            } else {
                let orig: String = conn
                    .query_row(
                        "SELECT name FROM nodes WHERE id = ?1",
                        params![ref_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                conn.execute(
                    "INSERT INTO pathfix_state (workspace_id, kind, ref_id, new_name, original_name)
                     VALUES (?1, 'source', ?2, ?3, ?4)
                     ON CONFLICT(workspace_id, kind, ref_id)
                     DO UPDATE SET new_name = excluded.new_name",
                    params![workspace_id, ref_id, new_name, orig],
                )?;
            }
        }
    }
    Ok(())
}

/// Mark a leaf's branch resolved (or not).
pub fn set_resolved(
    conn: &Connection,
    workspace_id: i64,
    kind: &str,
    ref_id: i64,
    resolved: bool,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO pathfix_state (workspace_id, kind, ref_id, resolved)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id, kind, ref_id)
         DO UPDATE SET resolved = excluded.resolved",
        params![workspace_id, kind, ref_id, resolved as i64],
    )?;
    Ok(())
}

/// Virtual source-node renames (id, original, new) for the export guide.
pub fn source_renames(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT ref_id, original_name, new_name FROM pathfix_state
         WHERE workspace_id = ?1 AND kind = 'source' AND new_name != ''",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    rows.collect()
}
