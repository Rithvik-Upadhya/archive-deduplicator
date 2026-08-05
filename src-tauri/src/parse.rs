//! Convert an uploaded `tree -JDs --inodes --device --timefmt "%Y-%m-%d_%H:%M:%S"`
//! JSON document (or a live folder scan) into flattened rows in the `nodes` table.
//! Subtree size and file counts are precomputed while walking so the UI never has
//! to aggregate.

use crate::model::TreeNode;
use rusqlite::{Transaction, params};

/// An intermediate flattened node produced before insertion, so subtree totals
/// can be rolled up bottom-up before writing to the database.
pub struct FlatNode {
    pub parent_index: Option<usize>,
    pub name: String,
    pub rel_path: String,
    pub node_type: String,
    pub size: i64,
    pub mtime: Option<String>,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub depth: i64,
    pub subtree_size: i64,
    pub subtree_file_count: i64,
    pub inode_high: Option<i64>,
    pub link_target: Option<String>,
}

/// Result of flattening: the node list plus device-level totals.
pub struct Flattened {
    pub nodes: Vec<FlatNode>,
    pub total_size: i64,
    pub file_count: i64,
    pub root_dev: Option<i64>,
}

/// Parse a `tree` JSON string. The top-level is an array whose first element is
/// the root directory. The root itself is skipped so its children become the
/// top-level entries of the source (the root is usually an absolute mount path).
pub fn parse_tree_json(json: &str) -> Result<Flattened, String> {
    let roots: Vec<TreeNode> =
        serde_json::from_str(json).map_err(|e| format!("Invalid tree JSON: {e}"))?;

    let mut flat = Flattened {
        nodes: Vec::new(),
        total_size: 0,
        file_count: 0,
        root_dev: None,
    };

    // `tree` emits a trailing {"type":"report",...} element which has no name.
    for root in roots {
        if root.node_type == "report" {
            continue;
        }
        if flat.root_dev.is_none() {
            flat.root_dev = root.dev;
        }
        if let Some(children) = root.contents {
            for child in children {
                walk(&child, None, "", &mut flat);
            }
        }
    }

    rollup(&mut flat);
    Ok(flat)
}

/// Recursively flatten a single `TreeNode` and its descendants.
fn walk(node: &TreeNode, parent_index: Option<usize>, parent_path: &str, flat: &mut Flattened) {
    if node.node_type == "report" {
        return;
    }
    let rel_path = if parent_path.is_empty() {
        node.name.clone()
    } else {
        format!("{parent_path}/{}", node.name)
    };
    let depth = if parent_path.is_empty() {
        0
    } else {
        parent_path.matches('/').count() as i64 + 1
    };
    let size = node.size.unwrap_or(0);
    let index = flat.nodes.len();

    flat.nodes.push(FlatNode {
        parent_index,
        name: node.name.clone(),
        rel_path: rel_path.clone(),
        node_type: node.node_type.clone(),
        size,
        mtime: node.time.clone(),
        inode: node.inode,
        dev: node.dev,
        depth,
        subtree_size: 0,
        subtree_file_count: 0,
        inode_high: None,
        link_target: None,
    });

    if node.node_type == "file" {
        flat.total_size += size;
        flat.file_count += 1;
    }

    if let Some(children) = &node.contents {
        for child in children {
            walk(child, Some(index), &rel_path, flat);
        }
    }
}

/// Roll up subtree size and file counts bottom-up. Because children always come
/// after their parent in the flattened vector, iterating in reverse and adding
/// each node's totals to its parent produces correct aggregates in one pass.
fn rollup(flat: &mut Flattened) {
    for i in (0..flat.nodes.len()).rev() {
        let (own_size, own_is_file, parent) = {
            let n = &flat.nodes[i];
            (n.size, n.node_type == "file", n.parent_index)
        };
        {
            let n = &mut flat.nodes[i];
            if own_is_file {
                n.subtree_size += own_size;
                n.subtree_file_count += 1;
            }
        }
        let (st_size, st_count) = {
            let n = &flat.nodes[i];
            (n.subtree_size, n.subtree_file_count)
        };
        if let Some(p) = parent {
            let pn = &mut flat.nodes[p];
            pn.subtree_size += st_size;
            pn.subtree_file_count += st_count;
        }
    }
}

/// Insert a flattened node list into the database under `source_id`, resolving
/// parent indices to real row ids. Runs inside the caller's transaction.
pub fn insert_nodes(tx: &Transaction, source_id: i64, flat: &Flattened) -> rusqlite::Result<()> {
    let mut ids: Vec<i64> = Vec::with_capacity(flat.nodes.len());
    {
        let mut stmt = tx.prepare(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, mtime, inode, dev, depth, subtree_size, subtree_file_count, inode_high, link_target)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )?;
        for n in &flat.nodes {
            let parent_id = n.parent_index.map(|pi| ids[pi]);
            stmt.execute(params![
                source_id,
                parent_id,
                n.name,
                n.rel_path,
                n.node_type,
                n.size,
                n.mtime,
                n.inode,
                n.dev,
                n.depth,
                n.subtree_size,
                n.subtree_file_count,
                n.inode_high,
                n.link_target,
            ])?;
            ids.push(tx.last_insert_rowid());
        }
    }
    Ok(())
}
