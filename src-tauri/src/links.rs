//! Hardlink alias-set detection: two file nodes that share `(dev, inode)`
//! within one source are the same physical file with two names. Runs as a
//! post-processing step right after `insert_nodes`, not inside `scan.rs`/
//! `parse.rs` -- those stay untouched so the scan pipeline and JSON-import
//! parsing are unaffected.

use rusqlite::{Transaction, params};
use std::collections::HashMap;

/// Whether this platform's live folder scan produces inode/dev values
/// reliable enough to trust for hardlink collapse. Unix `MetadataExt::ino()`/
/// `dev()` are real kernel-assigned identifiers, even on a FAT-mounted
/// volume. Windows is treated as untrusted for now: its existing
/// `scan.rs::inode_dev` already uses a genuinely stable identifier (the NTFS
/// file reference number, not the unreliable directory-offset `FileIndex`),
/// but FAT32/exFAT is extremely common for the external/USB drives this app
/// targets and there is no per-volume filesystem-type detection yet to tell
/// NTFS/ReFS apart from FAT/exFAT on that platform. `tree` JSON imports are
/// always untrusted (no platform signal at all, and the import is a
/// permanent record with no second pass).
#[cfg(unix)]
fn scan_produces_trusted_inodes() -> bool {
    true
}
#[cfg(not(unix))]
fn scan_produces_trusted_inodes() -> bool {
    false
}

struct MiniNode {
    id: i64,
    parent_id: Option<i64>,
    is_file: bool,
    size: i64,
    alias_of: Option<i64>,
    depth: i64,
}

/// Detect hardlink alias sets for `source_id`, mark `inode_trusted`, collapse
/// any trusted alias sets (setting `alias_of`, writing a `kind='hardlink'`
/// match group per set), recompute `subtree_size`/`subtree_file_count`
/// excluding aliases from ancestor totals, and refresh `sources.alias_bytes`/
/// `physical_size`. Returns the new `(physical_size, alias_bytes)` so the
/// caller can populate the `Source` it returns to the frontend without a
/// second round-trip query.
///
/// Must run inside the same transaction as the `insert_nodes` call that just
/// created `source_id`, before commit.
pub fn collapse_hardlinks_and_recompute(
    tx: &Transaction,
    source_id: i64,
) -> rusqlite::Result<(i64, i64)> {
    let kind: String = tx.query_row(
        "SELECT kind FROM sources WHERE id = ?1",
        params![source_id],
        |r| r.get(0),
    )?;
    let trusted = kind == "scan" && scan_produces_trusted_inodes();
    tx.execute(
        "UPDATE nodes SET inode_trusted = ?1 WHERE source_id = ?2",
        params![trusted as i64, source_id],
    )?;

    if trusted {
        collapse(tx, source_id)?;
    }
    // Always run, even when untrusted: with no `alias_of` ever set, this
    // simply reproduces the totals `insert_nodes` already wrote -- one code
    // path, negligible one-time cost per import, instead of branching.
    recompute_subtree_totals(tx, source_id)?;

    let alias_bytes: i64 = tx.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM nodes WHERE source_id = ?1 AND alias_of IS NOT NULL",
        params![source_id],
        |r| r.get(0),
    )?;
    tx.execute(
        "UPDATE sources SET alias_bytes = ?1, physical_size = total_size - ?1 WHERE id = ?2",
        params![alias_bytes, source_id],
    )?;
    let physical_size: i64 = tx.query_row(
        "SELECT physical_size FROM sources WHERE id = ?1",
        params![source_id],
        |r| r.get(0),
    )?;
    Ok((physical_size, alias_bytes))
}

/// Find alias sets (files sharing `(dev, inode)` within this source) and
/// collapse each: the lexicographically-lowest `rel_path` becomes canonical
/// (deterministic across re-scans), the rest get `alias_of` set to it. Each
/// set also gets a `kind='hardlink', confidence=100` match group covering
/// every member (canonical included), so browsing to any alias's "locate
/// duplicate" surfaces the whole set.
fn collapse(tx: &Transaction, source_id: i64) -> rusqlite::Result<()> {
    let workspace_id: i64 = tx.query_row(
        "SELECT workspace_id FROM sources WHERE id = ?1",
        params![source_id],
        |r| r.get(0),
    )?;

    struct Row {
        id: i64,
        dev: i64,
        inode: i64,
        size: i64,
    }
    let rows: Vec<Row> = {
        let mut stmt = tx.prepare(
            "SELECT id, dev, inode, size FROM nodes
             WHERE source_id = ?1 AND type = 'file' AND inode IS NOT NULL AND dev IS NOT NULL
             ORDER BY dev, inode, rel_path",
        )?;
        stmt.query_map(params![source_id], |r| {
            Ok(Row {
                id: r.get(0)?,
                dev: r.get(1)?,
                inode: r.get(2)?,
                size: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    let mut i = 0;
    while i < rows.len() {
        let mut j = i + 1;
        while j < rows.len() && rows[j].dev == rows[i].dev && rows[j].inode == rows[i].inode {
            j += 1;
        }
        if j - i >= 2 {
            let canonical = &rows[i];
            for alias in &rows[i + 1..j] {
                tx.execute(
                    "UPDATE nodes SET alias_of = ?1 WHERE id = ?2",
                    params![canonical.id, alias.id],
                )?;
            }
            tx.execute(
                "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
                 VALUES (?1, 'hardlink', 100.0, 'inode', ?2)",
                params![workspace_id, canonical.size],
            )?;
            let group_id = tx.last_insert_rowid();
            let mut stmt = tx.prepare(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            )?;
            for row in &rows[i..j] {
                stmt.execute(params![group_id, row.id])?;
            }
        }
        i = j;
    }
    Ok(())
}

/// Recompute `subtree_size`/`subtree_file_count` for every node under
/// `source_id`, excluding aliased files' contribution to their *ancestors*
/// (an aliased file's own row still reports its real size -- only the
/// double-counted propagation into containing directories is skipped, so a
/// folder's total isn't inflated by counting the same physical bytes twice).
/// Mirrors the reverse-depth-order accumulation in `scan.rs`'s local
/// `rollup()`, but reads from the `nodes` table directly: `insert_nodes`
/// already wrote these totals before `alias_of` existed (computed in-memory,
/// over `FlatNode`s with no concept of aliasing), so they need redoing here.
fn recompute_subtree_totals(tx: &Transaction, source_id: i64) -> rusqlite::Result<()> {
    let nodes: Vec<MiniNode> = {
        let mut stmt = tx.prepare(
            "SELECT id, parent_id, type, size, alias_of, depth FROM nodes WHERE source_id = ?1",
        )?;
        stmt.query_map(params![source_id], |r| {
            let node_type: String = r.get(2)?;
            Ok(MiniNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                is_file: node_type == "file",
                size: r.get(3)?,
                alias_of: r.get(4)?,
                depth: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    let pos_by_id: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();

    let mut subtree_size = vec![0i64; nodes.len()];
    let mut subtree_count = vec![0i64; nodes.len()];
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(nodes[i].depth));

    for i in order {
        if nodes[i].is_file {
            subtree_size[i] += nodes[i].size;
            subtree_count[i] += 1;
        }
        // Aliases still contribute to their OWN row (above) so the file
        // browser shows correct info for that individual node; they're just
        // excluded from being double-counted into any ancestor directory.
        if nodes[i].alias_of.is_none()
            && let Some(pid) = nodes[i].parent_id
            && let Some(&pi) = pos_by_id.get(&pid)
        {
            let (sz, cnt) = (subtree_size[i], subtree_count[i]);
            subtree_size[pi] += sz;
            subtree_count[pi] += cnt;
        }
    }

    let mut stmt =
        tx.prepare("UPDATE nodes SET subtree_size = ?1, subtree_file_count = ?2 WHERE id = ?3")?;
    for (i, n) in nodes.iter().enumerate() {
        stmt.execute(params![subtree_size[i], subtree_count[i], n.id])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, parse};
    use rusqlite::Connection;

    /// Build an in-memory workspace with one `kind`-typed source from `json`,
    /// returning (conn, workspace_id, source_id).
    fn setup(kind: &str, json: &str) -> (Connection, i64, i64) {
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
             VALUES (?1, ?2, 'disc-a', 'disc-a', 't', ?3, ?4)",
            params![ws, kind, flat.total_size, flat.file_count],
        )
        .unwrap();
        let source_id = tx.last_insert_rowid();
        parse::insert_nodes(&tx, source_id, &flat).unwrap();
        tx.commit().unwrap();

        (conn, ws, source_id)
    }

    fn node_id(conn: &Connection, source_id: i64, rel_path: &str) -> i64 {
        conn.query_row(
            "SELECT id FROM nodes WHERE source_id = ?1 AND rel_path = ?2",
            params![source_id, rel_path],
            |r| r.get(0),
        )
        .unwrap()
    }

    // Two nodes sharing (dev=10, inode=5): "photos/b.jpg" sorts after
    // "photos/a.jpg", so "a.jpg" should become canonical.
    const HARDLINK_TREE: &str = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
        {"type":"directory","name":"photos","inode":1,"dev":10,"contents":[
            {"type":"file","name":"a.jpg","inode":5,"dev":10,"size":1000,"time":"2024-01-01_10:00:00"},
            {"type":"file","name":"b.jpg","inode":5,"dev":10,"size":1000,"time":"2024-01-01_10:00:00"},
            {"type":"file","name":"unique.jpg","inode":6,"dev":10,"size":50,"time":"2024-01-01_10:00:00"}
        ]}
    ]}]"#;

    #[test]
    fn collapse_selects_lowest_rel_path_as_canonical() {
        // `kind` must be "scan" to be trusted on this (Unix) test host --
        // this exercises the real trust decision, not a JSON import.
        let (mut conn, _ws, source_id) = setup("scan", HARDLINK_TREE);
        let a = node_id(&conn, source_id, "photos/a.jpg");
        let b = node_id(&conn, source_id, "photos/b.jpg");

        let tx = conn.transaction().unwrap();
        collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        let alias_of_b: Option<i64> = conn
            .query_row(
                "SELECT alias_of FROM nodes WHERE id = ?1",
                params![b],
                |r| r.get(0),
            )
            .unwrap();
        let alias_of_a: Option<i64> = conn
            .query_row(
                "SELECT alias_of FROM nodes WHERE id = ?1",
                params![a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            alias_of_b,
            Some(a),
            "b.jpg should point at a.jpg as canonical"
        );
        assert_eq!(alias_of_a, None, "the canonical itself has no alias_of");
    }

    #[test]
    fn physical_size_excludes_alias_bytes() {
        let (mut conn, _ws, source_id) = setup("scan", HARDLINK_TREE);
        let total_size: i64 = conn
            .query_row(
                "SELECT total_size FROM sources WHERE id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();

        let tx = conn.transaction().unwrap();
        let (physical_size, alias_bytes) =
            collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        assert_eq!(alias_bytes, 1000, "one alias of a 1000-byte file");
        assert_eq!(physical_size, total_size - 1000);
    }

    #[test]
    fn subtree_size_excludes_alias_from_ancestor_but_not_from_own_row() {
        let (mut conn, _ws, source_id) = setup("scan", HARDLINK_TREE);
        let photos = node_id(&conn, source_id, "photos");
        let b = node_id(&conn, source_id, "photos/b.jpg");

        let tx = conn.transaction().unwrap();
        collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        // photos/ contains a.jpg (1000, canonical) + b.jpg (1000, alias,
        // excluded from propagation) + unique.jpg (50) => 1050, not 2050.
        let (photos_size, photos_count): (i64, i64) = conn
            .query_row(
                "SELECT subtree_size, subtree_file_count FROM nodes WHERE id = ?1",
                params![photos],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(photos_size, 1050);
        assert_eq!(photos_count, 2);

        // b.jpg's own row still shows its real size/count.
        let (b_size, b_count): (i64, i64) = conn
            .query_row(
                "SELECT subtree_size, subtree_file_count FROM nodes WHERE id = ?1",
                params![b],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(b_size, 1000, "alias's own row keeps its real size");
        assert_eq!(b_count, 1);
    }

    #[test]
    fn duplicated_size_by_source_reports_zero_for_hardlink_only_set() {
        let (mut conn, ws, source_id) = setup("scan", HARDLINK_TREE);
        let tx = conn.transaction().unwrap();
        collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        // Run the real matcher too: aliases must never reach a kind='file'
        // group, so there is nothing for duplicated_size_by_source to count.
        crate::dedup::run_with_progress(
            &mut conn,
            ws,
            crate::dedup::DedupParams::default(),
            |_, _, _| {},
        )
        .unwrap();
        let dup = crate::rollup::duplicated_size_by_source(&conn, ws).unwrap();
        assert_eq!(dup.get(&source_id).copied().unwrap_or(0), 0);
    }

    #[test]
    fn untrusted_source_does_not_collapse_hardlinks() {
        // A `tree` JSON import is always untrusted, regardless of host OS.
        let (mut conn, _ws, source_id) = setup("json", HARDLINK_TREE);
        let total_size: i64 = conn
            .query_row(
                "SELECT total_size FROM sources WHERE id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();

        let tx = conn.transaction().unwrap();
        let (physical_size, alias_bytes) =
            collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        assert_eq!(alias_bytes, 0);
        assert_eq!(physical_size, total_size);
        let b = node_id(&conn, source_id, "photos/b.jpg");
        let alias_of_b: Option<i64> = conn
            .query_row(
                "SELECT alias_of FROM nodes WHERE id = ?1",
                params![b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alias_of_b, None);
    }

    #[test]
    fn dedup_rerun_does_not_delete_hardlink_groups() {
        let (mut conn, ws, source_id) = setup("scan", HARDLINK_TREE);
        let tx = conn.transaction().unwrap();
        collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        let hardlink_groups_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'hardlink'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hardlink_groups_before, 1);

        crate::dedup::run_with_progress(
            &mut conn,
            ws,
            crate::dedup::DedupParams::default(),
            |_, _, _| {},
        )
        .unwrap();

        let hardlink_groups_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'hardlink'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hardlink_groups_after, 1,
            "a dedup rerun must not delete the structural hardlink group"
        );
    }
}
