//! Hardlink alias-set detection: two file nodes that share `(dev, inode)`
//! within one source are the same physical file with two names. Runs as a
//! post-processing step right after `insert_nodes`, not inside `scan.rs`/
//! `parse.rs` -- those stay untouched so the scan pipeline and JSON-import
//! parsing are unaffected.

use crate::medium;
use rusqlite::{Transaction, params};
use std::collections::HashMap;

struct MiniNode {
    id: i64,
    parent_id: Option<i64>,
    /// `type == "file"` -- gates the *byte* contribution. A symlink holds no
    /// content bytes of its own, so it is a leaf without being a file here.
    is_file: bool,
    /// `type` is `file` or `link` -- gates the *name* contribution. Both are
    /// things the user finds when listing the directory by hand.
    is_leaf: bool,
    size: i64,
    alias_of: Option<i64>,
    depth: i64,
}

/// Detect hardlink alias sets for `source_id`, mark `inode_trusted`, collapse
/// any trusted alias sets (setting `alias_of`, writing a `kind='hardlink'`
/// match group per set), recompute `subtree_size`/`subtree_file_count`
/// excluding alias *bytes* (but not alias *names*) from ancestor totals, and
/// refresh `sources.alias_bytes`/`physical_size`.
/// Returns the new `(physical_size, alias_bytes)` so the
/// caller can populate the `Source` it returns to the frontend without a
/// second round-trip query.
///
/// Must run inside the same transaction as the `insert_nodes` call that just
/// created `source_id`, before commit.
pub fn collapse_hardlinks_and_recompute(
    tx: &Transaction,
    source_id: i64,
) -> rusqlite::Result<(i64, i64)> {
    let (kind, filesystem): (String, Option<String>) = tx.query_row(
        "SELECT kind, filesystem FROM sources WHERE id = ?1",
        params![source_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let trusted = kind == "scan" && medium::filesystem_is_trusted(filesystem.as_deref());
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
        inode_high: Option<i64>,
        size: i64,
    }
    let rows: Vec<Row> = {
        let mut stmt = tx.prepare(
            "SELECT id, dev, inode, inode_high, size FROM nodes
             WHERE source_id = ?1 AND type = 'file' AND inode IS NOT NULL AND dev IS NOT NULL
             ORDER BY dev, inode, inode_high, rel_path",
        )?;
        stmt.query_map(params![source_id], |r| {
            Ok(Row {
                id: r.get(0)?,
                dev: r.get(1)?,
                inode: r.get(2)?,
                inode_high: r.get(3)?,
                size: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    // The full identity is (dev, inode, inode_high) -- `inode_high` is the
    // high 64 bits of a ReFS 128-bit file ID (see db.rs) and is NULL on every
    // other filesystem, where the 64-bit `inode` alone is the whole ID.
    let mut i = 0;
    while i < rows.len() {
        let mut j = i + 1;
        while j < rows.len()
            && rows[j].dev == rows[i].dev
            && rows[j].inode == rows[i].inode
            && rows[j].inode_high == rows[i].inode_high
        {
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
/// `source_id`. The two axes are treated differently on purpose, per "counts
/// count names; sizes count bytes held":
///
/// - **Bytes** from an aliased file do *not* propagate to its ancestors. They
///   are a second name for bytes the canonical already contributed, so a
///   folder's total would otherwise be inflated by counting the same physical
///   bytes twice. (The alias's own row still reports its real size.)
/// - **Names** from an aliased file *do* propagate. Someone listing the folder
///   by hand sees every name, and must not find a different count than the app
///   reported.
///
/// Mirrors the reverse-depth-order accumulation in `scan.rs`'s local
/// `rollup()`, but reads from the `nodes` table directly: `insert_nodes`
/// already wrote these totals before `alias_of` existed (computed in-memory,
/// over `FlatNode`s with no concept of aliasing), so they need redoing here.
pub(crate) fn recompute_subtree_totals(tx: &Transaction, source_id: i64) -> rusqlite::Result<()> {
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
                is_leaf: node_type == "file" || node_type == "link",
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
        }
        if nodes[i].is_leaf {
            subtree_count[i] += 1;
        }
        // An alias propagates its *count* but not its *bytes*.
        //
        // Count, because a count counts names: the user browsing this
        // directory sees the alias sitting there, and must not find more files
        // by hand than the app reported.
        //
        // Bytes, never: an alias is a second name for bytes the canonical
        // already contributed, so adding them would inflate every ancestor and
        // make `subtree_size` stop meaning "what this subtree occupies" --
        // deleting k-1 aliases frees nothing.
        if let Some(pid) = nodes[i].parent_id
            && let Some(&pi) = pos_by_id.get(&pid)
        {
            let (sz, cnt) = (subtree_size[i], subtree_count[i]);
            if nodes[i].alias_of.is_none() {
                subtree_size[pi] += sz;
            }
            subtree_count[pi] += cnt;
        }
    }

    // One `UPDATE` per node (~337k on the largest real corpus this app
    // targets), but the statement is prepared once above and every execute
    // shares this single transaction -- the two costs actual per-statement
    // overhead comes from (SQL re-parsing, transaction fsync). Chunking into
    // savepoints or a temp-table `UPDATE ... FROM` would add real complexity
    // for a gain that hasn't been measured to exist on top of that; revisit
    // only if profiling a real large scan shows this loop dominating.
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
        setup_with_filesystem(kind, Some("ext4"), json)
    }

    /// Like `setup`, but with an explicit (possibly untrusted/absent)
    /// filesystem -- trust is now `kind == "scan" && filesystem_is_trusted`,
    /// so exercising the untrusted-filesystem path needs control over both.
    fn setup_with_filesystem(
        kind: &str,
        filesystem: Option<&str>,
        json: &str,
    ) -> (Connection, i64, i64) {
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
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count, filesystem)
             VALUES (?1, ?2, 'disc-a', 'disc-a', 't', ?3, ?4, ?5)",
            params![ws, kind, flat.total_size, flat.file_count, filesystem],
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
    fn alias_counts_propagate_to_ancestors_but_alias_bytes_do_not() {
        // The names/bytes split. `photos` holds three names -- a.jpg, b.jpg
        // (a hardlink alias of a.jpg) and unique.jpg -- but only two files'
        // worth of bytes, since the alias shares a.jpg's.
        let (mut conn, _ws, source_id) = setup("scan", HARDLINK_TREE);
        let tx = conn.transaction().unwrap();
        collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        let photos = node_id(&conn, source_id, "photos");
        let (count, size): (i64, i64) = conn
            .query_row(
                "SELECT subtree_file_count, subtree_size FROM nodes WHERE id = ?1",
                params![photos],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            count, 3,
            "every name counts -- the user sees three files in photos/"
        );
        assert_eq!(
            size, 1050,
            "the alias adds no bytes: 1000 (a.jpg, shared with b.jpg) + 50"
        );
    }

    #[test]
    fn collapse_selects_lowest_rel_path_as_canonical() {
        // `kind` must be "scan" on a trusted filesystem (e.g. ext4, the
        // `setup` default) to exercise the real trust decision, not a JSON
        // import or an untrusted filesystem like exFAT.
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

        // photos/ contains a.jpg (1000, canonical) + b.jpg (1000, alias) +
        // unique.jpg (50). The two axes diverge here: b.jpg's *bytes* are a
        // second name for a.jpg's, so they never propagate => 1050, not 2050;
        // b.jpg's *name* is one the user can see in the folder, so it does
        // propagate => 3, not 2.
        let (photos_size, photos_count): (i64, i64) = conn
            .query_row(
                "SELECT subtree_size, subtree_file_count FROM nodes WHERE id = ?1",
                params![photos],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(photos_size, 1050, "alias bytes never reach the ancestor");
        assert_eq!(photos_count, 3, "every name counts, alias included");

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
    fn untrusted_filesystem_still_gates_hardlink_collapse() {
        // A live scan of a FAT/exFAT volume must stay untrusted even though
        // `kind == "scan"` -- the filesystem itself is what's untrusted here,
        // not the platform. This is the real-detection replacement for the
        // old blanket `cfg(unix)` trust check.
        let (mut conn, _ws, source_id) =
            setup_with_filesystem("scan", Some("exFAT"), HARDLINK_TREE);
        let tx = conn.transaction().unwrap();
        let (physical_size, alias_bytes) =
            collapse_hardlinks_and_recompute(&tx, source_id).unwrap();
        tx.commit().unwrap();

        assert_eq!(alias_bytes, 0);
        let total_size: i64 = conn
            .query_row(
                "SELECT total_size FROM sources WHERE id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
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
    fn refs_128_bit_ids_only_collapse_when_both_halves_match() {
        // Two files share the same 64-bit `inode` but differ in the ReFS
        // high-order half -- they must NOT be treated as the same physical
        // file. This is the correctness reason inode_high is stored as a
        // separate column rather than folding a 128-bit ID into one i64.
        //
        // No real ReFS volume is available in this dev/CI environment, so
        // `inode_high` is set by hand here rather than produced by an actual
        // scan; `scan_win::read_dir_ex`'s split of `FILE_ID_128` is what
        // populates it for real (Stage 4.1/4.2), and that production path is
        // exercised only by `scan.rs`'s
        // `scan_produces_identical_flat_nodes_as_the_walkdir_path` parity
        // test, which needs a real Windows target to run.
        let (mut conn, _ws, source_id) = setup("scan", HARDLINK_TREE);
        let a = node_id(&conn, source_id, "photos/a.jpg");
        let b = node_id(&conn, source_id, "photos/b.jpg");
        conn.execute("UPDATE nodes SET inode_high = 1 WHERE id = ?1", params![a])
            .unwrap();
        conn.execute("UPDATE nodes SET inode_high = 2 WHERE id = ?1", params![b])
            .unwrap();

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
        assert_eq!(
            alias_of_b, None,
            "differing inode_high halves must not collapse together"
        );
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
