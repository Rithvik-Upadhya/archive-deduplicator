//! The duplicate-matching engine. Because we never have file contents, matching
//! relies entirely on metadata, structured as absolute vetoes followed by
//! fixed-confidence evidence tiers (never a continuous score, never merged
//! across tiers) -- see the module's tier/veto documentation below. Hardlink
//! aliases (see `links.rs`) are excluded upstream by `load_files` and never
//! become matcher candidates at all.

use rusqlite::{Connection, params};
use std::collections::{BTreeSet, HashMap};

/// A file node loaded for matching.
struct FileRow {
    id: i64,
    parent_id: Option<i64>,
    name: String,
    size: i64,
    mtime: Option<String>,
    parent_name: String,
}

/// Tunable parameters supplied from the UI.
#[derive(Clone, Copy)]
pub struct DedupParams {
    /// Files strictly smaller than this many bytes are excluded from matching
    /// entirely (hard filter — they generate size-collision noise). 64 KiB:
    /// on a typical archive, files below this account for the large majority
    /// of file *count* but a negligible share of *bytes*, so there is little
    /// value in pairwise-scoring them.
    pub min_size_bytes: i64,
    /// Groups whose tier confidence is below this are not persisted.
    pub min_confidence: f64,
}

impl Default for DedupParams {
    fn default() -> Self {
        DedupParams {
            min_size_bytes: 65536,
            min_confidence: 40.0,
        }
    }
}

/// A resolved match group never merges across tiers and never averages a
/// score: every member pair within a group independently qualifies at
/// exactly this tier (the "clique requirement" -- see `run_with_progress`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Size + exact name + exact/near (<=2s) mtime match.
    C,
    /// Size + exact name, mtime differs or is absent.
    D,
    /// Size + same parent-folder name + some mtime support, names differ.
    E,
}

impl Tier {
    fn confidence(self) -> f64 {
        match self {
            Tier::C => 70.0,
            Tier::D => 55.0,
            Tier::E => 45.0,
        }
    }
    fn signal(self) -> &'static str {
        match self {
            Tier::C => "name+mtime",
            Tier::D => "name",
            Tier::E => "parent+mtime",
        }
    }
}

/// A resolved match group never persists with more than this many members:
/// an unusually large cluster at a pure-metadata tier is more likely
/// systematic noise (e.g. many fixed-size files) than a genuine duplicate
/// set, and per this app's own false-positive-averse design, a missing group
/// only costs disk space -- an acceptable failure mode -- while persisting a
/// huge, likely-wrong mega-group would be actively misleading.
const MAX_GROUP_SIZE: usize = 32;

/// Safety cap on how many same-(size, extension) candidates get pairwise-
/// compared at all. 2000^2 = 4M comparisons is not actually slow in Rust;
/// this exists to name and bound a pathological case (e.g. very many
/// identically-sized files) rather than because realistic scoring is
/// expensive.
const BUCKET_SAFETY_CAP: usize = 2000;

/// The evidence support an mtime comparison provides. `None` is deliberately
/// *not* a veto: a reset or unrelated mtime must never block a match on its
/// own, only fail to add support for one (a mtime can be lost or reset by
/// careless copy tools without the underlying file having changed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MtimeMatch {
    /// Identical string, or within 2 seconds (FAT/exFAT's 2s granularity).
    Strong,
    /// An exact whole-hour offset (a timezone shift from a copy tool).
    Moderate,
    None,
}

fn parse_tree_time(s: &str) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d_%H:%M:%S").ok()
}

/// Compare two `tree`-format mtimes under the tolerances real cross-medium
/// copies actually produce: FAT/exFAT stores local time at 2s granularity
/// and NTFS stores UTC, and copy tools may preserve, shift by a whole-hour
/// offset, or reset the timestamp entirely.
fn compare_mtime(a: Option<&str>, b: Option<&str>) -> MtimeMatch {
    let (Some(a), Some(b)) = (a, b) else {
        return MtimeMatch::None;
    };
    if a == b {
        return MtimeMatch::Strong;
    }
    let (Some(ta), Some(tb)) = (parse_tree_time(a), parse_tree_time(b)) else {
        return MtimeMatch::None;
    };
    let diff = (ta - tb).num_seconds().abs();
    if diff <= 2 {
        return MtimeMatch::Strong;
    }
    if diff > 0 && diff % 3600 == 0 && diff <= 14 * 3600 {
        return MtimeMatch::Moderate;
    }
    MtimeMatch::None
}

/// Extension (lowercased) of a name, or empty if it has none.
fn ext(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Enumerate every maximal clique (size >= 2) of `vertices` under `edge`,
/// via the simplest (no-pivot) form of Bron-Kerbosch. Vertex sets here are
/// always small (bounded by `BUCKET_SAFETY_CAP` and, in practice, far
/// smaller -- a same-size/extension/parent-folder cluster), so the classic
/// worst-case complexity of clique enumeration is a non-issue.
fn maximal_cliques(vertices: &[usize], edge: impl Fn(usize, usize) -> bool) -> Vec<Vec<usize>> {
    let mut adj: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    for &v in vertices {
        adj.entry(v).or_default();
    }
    for i in 0..vertices.len() {
        for j in (i + 1)..vertices.len() {
            let (a, b) = (vertices[i], vertices[j]);
            if edge(a, b) {
                adj.get_mut(&a).unwrap().insert(b);
                adj.get_mut(&b).unwrap().insert(a);
            }
        }
    }
    let p: BTreeSet<usize> = vertices.iter().copied().collect();
    let mut cliques = Vec::new();
    bron_kerbosch(&mut Vec::new(), p, BTreeSet::new(), &adj, &mut cliques);
    cliques
}

fn bron_kerbosch(
    r: &mut Vec<usize>,
    mut p: BTreeSet<usize>,
    mut x: BTreeSet<usize>,
    adj: &HashMap<usize, BTreeSet<usize>>,
    cliques: &mut Vec<Vec<usize>>,
) {
    if p.is_empty() && x.is_empty() {
        if r.len() >= 2 {
            cliques.push(r.clone());
        }
        return;
    }
    for v in p.clone() {
        let neighbors = &adj[&v];
        r.push(v);
        bron_kerbosch(
            r,
            p.intersection(neighbors).copied().collect(),
            x.intersection(neighbors).copied().collect(),
            adj,
            cliques,
        );
        r.pop();
        p.remove(&v);
        x.insert(v);
    }
}

/// Every pair among `group` (indices into `files`) satisfies `Strong` mtime
/// support. Used to decide whether an exact-name group promotes from tier D
/// to the stricter tier C.
fn all_pairs_strong_mtime(files: &[FileRow], group: &[usize]) -> bool {
    for i in 0..group.len() {
        for j in (i + 1)..group.len() {
            if compare_mtime(
                files[group[i]].mtime.as_deref(),
                files[group[j]].mtime.as_deref(),
            ) != MtimeMatch::Strong
            {
                return false;
            }
        }
    }
    true
}

/// Resolve every metadata-tier match group within one (size, extension)
/// sub-bucket. Two independent shapes of evidence, never mixed:
///
/// - **Tiers C/D** group by *exact* name -- string equality is transitive,
///   so this never needs a clique check for membership; a group either
///   promotes to C (every pair also has `Strong` mtime support) or stays at
///   the weaker D (mtime differs or is absent for at least one pair).
/// - **Tier E** groups by same parent-folder name (also transitive), but the
///   "names differ AND some mtime support" edge condition is *not*
///   transitive (the mtime tolerances aren't equivalence relations), so
///   candidates within a parent cluster go through real maximal-clique
///   enumeration -- this is what stops a stray weak pair from silently
///   dragging two unrelated files into the same group (the same failure
///   union-find had).
fn resolve_groups_in_bucket(files: &[FileRow], idxs: &[usize]) -> Vec<(Tier, Vec<usize>)> {
    let mut out = Vec::new();

    let mut by_ext: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in idxs {
        by_ext.entry(ext(&files[i].name)).or_default().push(i);
    }

    for sub in by_ext.into_values() {
        if sub.len() < 2 || sub.len() > BUCKET_SAFETY_CAP {
            continue;
        }

        // Tiers C/D: exact name.
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for &i in &sub {
            by_name.entry(files[i].name.as_str()).or_default().push(i);
        }
        for group in by_name.into_values() {
            if group.len() < 2 || group.len() > MAX_GROUP_SIZE {
                continue;
            }
            let tier = if all_pairs_strong_mtime(files, &group) {
                Tier::C
            } else {
                Tier::D
            };
            out.push((tier, group));
        }

        // Tier E: same parent-folder name, names differ, some mtime support.
        let mut by_parent: HashMap<String, Vec<usize>> = HashMap::new();
        for &i in &sub {
            if files[i].parent_name.is_empty() {
                continue;
            }
            by_parent
                .entry(files[i].parent_name.to_lowercase())
                .or_default()
                .push(i);
        }
        for cluster in by_parent.into_values() {
            if cluster.len() < 2 || cluster.len() > BUCKET_SAFETY_CAP {
                continue;
            }
            let cliques = maximal_cliques(&cluster, |a, b| {
                files[a].name != files[b].name
                    && compare_mtime(files[a].mtime.as_deref(), files[b].mtime.as_deref())
                        != MtimeMatch::None
            });
            for clique in cliques {
                if clique.len() > MAX_GROUP_SIZE {
                    continue;
                }
                out.push((Tier::E, clique));
            }
        }
    }

    out
}

/// Run the full dedup pass for a workspace: clears prior groups, rebuilds file
/// groups, then folder-level groups, all inside one transaction.
///
/// The extension veto is applied by sub-bucketing on it (see
/// `resolve_groups_in_bucket`); a file's own veto conditions -- being a
/// hardlink alias or a symlink -- never apply here at all, because
/// `load_files` excludes both from the candidate pool entirely (aliases via
/// `alias_of IS NULL`, symlinks via `type = 'file'`). A file may legitimately
/// end up in more than one persisted group across tiers (e.g. a tier-D match
/// on exact name with one partner, and a separate tier-E match on parent
/// folder with a different partner) -- this reflects two independent pieces
/// of partial evidence, not a bug; `get_group_for_node` already resolves
/// "which one to show" via `ORDER BY confidence DESC LIMIT 1`.
///
/// Calls `on_phase(name, current, total)` at each of the five natural phase
/// boundaries below (plus a final "done" tick), so a caller with a progress
/// UI can report something better than silence during a long pass. Kept as a
/// generic closure rather than a `tauri`-specific type so this module stays
/// free of any Tauri dependency -- `commands.rs` is the only place that turns
/// a tick into an emitted event. Tests that don't care about progress pass a
/// no-op closure.
pub fn run_with_progress(
    conn: &mut Connection,
    workspace_id: i64,
    params: DedupParams,
    mut on_phase: impl FnMut(&str, u64, u64),
) -> rusqlite::Result<usize> {
    const TOTAL_PHASES: u64 = 5;

    // Load parent-name lookup and all files for the workspace.
    on_phase("loading", 0, TOTAL_PHASES);
    let parent_names = load_parent_names(conn, workspace_id)?;
    let files = load_files(conn, workspace_id, &parent_names)?;

    // Bucket file indices by size. Files below the tunable minimum, and
    // zero-byte files unconditionally (an absolute veto, not merely a
    // penalty: any two empty files look identical regardless of every other
    // signal), are excluded entirely.
    let mut by_size: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size < params.min_size_bytes || f.size == 0 {
            continue;
        }
        by_size.entry(f.size).or_default().push(i);
    }

    on_phase("matching", 1, TOTAL_PHASES);
    let mut groups: Vec<(Tier, Vec<usize>)> = Vec::new();
    for idxs in by_size.values() {
        if idxs.len() < 2 {
            continue;
        }
        groups.extend(resolve_groups_in_bucket(&files, idxs));
    }

    on_phase("grouping", 2, TOTAL_PHASES);
    let tx = conn.transaction()?;
    // Clear previous results for this workspace, but never the structural
    // hardlink groups `links.rs` writes at import time -- those aren't a
    // tunable match this pass owns, and rebuilding this workspace's dedup
    // results must not erase them.
    tx.execute(
        "DELETE FROM match_groups WHERE workspace_id = ?1 AND kind != 'hardlink'",
        params![workspace_id],
    )?;

    let mut group_count = 0usize;
    for (tier, members) in &groups {
        if tier.confidence() < params.min_confidence {
            continue;
        }
        let size = files[members[0]].size;

        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', ?2, ?3, ?4)",
            params![workspace_id, tier.confidence(), tier.signal(), size],
        )?;
        let group_id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            )?;
            for &i in members {
                stmt.execute(params![group_id, files[i].id])?;
            }
        }
        group_count += 1;
    }

    tx.commit()?;

    // Folder-level rollup uses the freshly written file groups.
    on_phase("folder_rollup", 3, TOTAL_PHASES);
    let folder_groups = super::rollup::build_folder_groups(conn, workspace_id)?;

    // Rebuild the per-node duplicate annotation cache so tree browsing is fast.
    on_phase("annotating", 4, TOTAL_PHASES);
    super::rollup::rebuild_annotations(conn, workspace_id)?;

    on_phase("done", TOTAL_PHASES, TOTAL_PHASES);
    Ok(group_count + folder_groups)
}

/// Load a map of node_id -> node name so parent folder names can be attached.
fn load_parent_names(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, String>> {
    let mut map = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, name) = row?;
        map.insert(id, name);
    }
    Ok(map)
}

/// Load all file nodes for a workspace with their parent folder name resolved.
/// Hardlink aliases (`alias_of IS NOT NULL`) are excluded: they're handled
/// structurally by `links.rs`, not as matcher candidates.
fn load_files(
    conn: &Connection,
    workspace_id: i64,
    parent_names: &HashMap<i64, String>,
) -> rusqlite::Result<Vec<FileRow>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id, n.name, n.size, n.mtime
         FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'file' AND s.excluded = 0 AND n.alias_of IS NULL",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        let parent_id: Option<i64> = r.get(1)?;
        Ok(FileRow {
            id: r.get(0)?,
            parent_id,
            name: r.get(2)?,
            size: r.get(3)?,
            mtime: r.get(4)?,
            parent_name: String::new(),
        })
    })?;
    let mut files = Vec::new();
    for row in rows {
        let mut f = row?;
        if let Some(pid) = f.parent_id {
            if let Some(name) = parent_names.get(&pid) {
                f.parent_name = name.clone();
            }
        }
        files.push(f);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, parse};

    /// Build an in-memory workspace with two sources importing the same tree.
    fn setup_two_identical_sources() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        // A small tree: folder "photos" with two sizable files.
        let json = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
            {"type":"directory","name":"photos","inode":1,"dev":10,"size":128,"time":"2024-01-01_10:00:00","contents":[
                {"type":"file","name":"a.jpg","inode":2,"dev":10,"size":500000,"time":"2024-01-01_10:00:00"},
                {"type":"file","name":"b.jpg","inode":3,"dev":10,"size":800000,"time":"2024-01-01_10:05:00"}
            ]}
        ]}]"#;

        for (label, dev) in [("disc-a", 10i64), ("disc-b", 20i64)] {
            let flat =
                parse::parse_tree_json(&json.replace("\"dev\":10", &format!("\"dev\":{dev}")))
                    .unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
                 VALUES (?1, 'json', ?2, ?2, 't', ?3, ?4)",
                params![ws, label, flat.total_size, flat.file_count],
            )
            .unwrap();
            let sid = tx.last_insert_rowid();
            parse::insert_nodes(&tx, sid, &flat).unwrap();
            tx.commit().unwrap();
        }
        conn
    }

    /// Build an in-memory workspace with a single source from raw file rows
    /// `(name, size, mtime, parent_name)`, all placed directly under
    /// distinct top-level parent directories named after `parent_name` (or
    /// no parent at all when `parent_name` is empty). Used by the tier/veto
    /// tests below, which don't need real cross-source duplication -- just
    /// precise control over name/size/mtime/parent combinations.
    fn setup_files(rows: &[(&str, i64, Option<&str>, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'json', 's', 's', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();

        let mut parent_ids: HashMap<&str, i64> = HashMap::new();
        for (name, size, mtime, parent_name) in rows {
            let parent_id = if parent_name.is_empty() {
                None
            } else {
                Some(*parent_ids.entry(parent_name).or_insert_with(|| {
                    conn.execute(
                        "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
                         VALUES (?1, NULL, ?2, ?2, 'directory', 0)",
                        params![source_id, parent_name],
                    )
                    .unwrap();
                    conn.last_insert_rowid()
                }))
            };
            conn.execute(
                "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, mtime)
                 VALUES (?1, ?2, ?3, ?3, 'file', ?4, ?5)",
                params![source_id, parent_id, name, size, mtime],
            )
            .unwrap();
        }
        conn
    }

    fn group_rows(conn: &Connection, ws: i64) -> Vec<(f64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT confidence, primary_signal FROM match_groups
                 WHERE workspace_id = ?1 AND kind = 'file' ORDER BY confidence DESC",
            )
            .unwrap();
        stmt.query_map(params![ws], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn detects_cross_source_file_duplicates() {
        let mut conn = setup_two_identical_sources();
        let ws = 1;
        let count = run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        // Expect at least the two file groups (a.jpg, b.jpg) plus a folder group.
        assert!(count >= 2, "expected duplicate groups, got {count}");

        let file_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'file'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(file_groups, 2, "two files should each form a group");

        let folder_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'folder'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(folder_groups, 1, "the photos folder should be flagged");
    }

    #[test]
    fn small_files_are_deprioritized() {
        let mut conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();
        // Two sources each with a tiny 10-byte file of the same name.
        let json = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
            {"type":"file","name":"tiny.txt","inode":2,"dev":10,"size":10,"time":"2024-01-01_10:00:00"}
        ]}]"#;
        for (label, dev) in [("a", 10i64), ("b", 20i64)] {
            let flat =
                parse::parse_tree_json(&json.replace("\"dev\":10", &format!("\"dev\":{dev}")))
                    .unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
                 VALUES (?1, 'json', ?2, ?2, 't', ?3, ?4)",
                params![ws, label, flat.total_size, flat.file_count],
            )
            .unwrap();
            let sid = tx.last_insert_rowid();
            parse::insert_nodes(&tx, sid, &flat).unwrap();
            tx.commit().unwrap();
        }
        // With the default 4KB threshold + high min confidence, tiny files drop out.
        let params = DedupParams {
            min_size_bytes: 4096,
            min_confidence: 60.0,
        };
        run_with_progress(&mut conn, ws, params, |_, _, _| {}).unwrap();
        let file_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'file'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(file_groups, 0, "tiny files should be deprioritized out");
    }

    #[test]
    fn excluded_source_is_left_out_of_matching() {
        let mut conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        // disc-a and disc-b share an identical file; disc-c holds something
        // unrelated, so it never factors into this test either way.
        let shared_json = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
            {"type":"file","name":"shared.jpg","inode":2,"dev":10,"size":500000,"time":"2024-01-01_10:00:00"}
        ]}]"#;
        let other_json = r#"[{"type":"directory","name":"/vol","dev":10,"contents":[
            {"type":"file","name":"other.jpg","inode":2,"dev":10,"size":900000,"time":"2024-01-01_10:00:00"}
        ]}]"#;

        let mut source_id = |label: &str, dev: i64, json: &str| -> i64 {
            let flat =
                parse::parse_tree_json(&json.replace("\"dev\":10", &format!("\"dev\":{dev}")))
                    .unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute(
                "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
                 VALUES (?1, 'json', ?2, ?2, 't', ?3, ?4)",
                params![ws, label, flat.total_size, flat.file_count],
            )
            .unwrap();
            let sid = tx.last_insert_rowid();
            parse::insert_nodes(&tx, sid, &flat).unwrap();
            tx.commit().unwrap();
            sid
        };
        let src_a = source_id("disc-a", 10, shared_json);
        let _src_b = source_id("disc-b", 20, shared_json);
        let _src_c = source_id("disc-c", 30, other_json);

        conn.execute(
            "UPDATE sources SET excluded = 1 WHERE id = ?1",
            params![src_a],
        )
        .unwrap();

        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();

        let a_member_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_members mm
                 JOIN nodes n ON n.id = mm.node_id
                 WHERE n.source_id = ?1",
                params![src_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            a_member_count, 0,
            "excluded source must have no match members"
        );

        // disc-b's copy of shared.jpg has no remaining partner (its only
        // match was on the excluded disc-a), so no file group should exist.
        let file_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'file'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            file_groups, 0,
            "disc-b's file should be unmatched once disc-a is excluded"
        );
    }

    #[test]
    fn veto_extension_mismatch_blocks_an_otherwise_strong_match() {
        // Same size, same stem, same mtime -- everything the old scored
        // model would have rewarded -- but different extensions.
        let mut conn = setup_files(&[
            ("video.mp4", 1_000_000, Some("2024-01-01_10:00:00"), ""),
            ("video.mov", 1_000_000, Some("2024-01-01_10:00:00"), ""),
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert!(
            group_rows(&conn, ws).is_empty(),
            "extension veto must block the match"
        );
    }

    #[test]
    fn exact_name_and_strong_mtime_forms_tier_c() {
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:01"), ""), // within 2s
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        let rows = group_rows(&conn, ws);
        assert_eq!(rows, vec![(70.0, "name+mtime".to_string())]);
    }

    #[test]
    fn exact_name_with_no_mtime_signal_forms_tier_d() {
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2026-06-15_03:22:10"), ""), // unrelated
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        let rows = group_rows(&conn, ws);
        assert_eq!(
            rows,
            vec![(55.0, "name".to_string())],
            "a wildly different mtime must downgrade to tier D, not veto the match"
        );
    }

    #[test]
    fn same_parent_different_name_with_moderate_mtime_forms_tier_e() {
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), "photos"),
            ("b.jpg", 500_000, Some("2024-01-01_13:00:00"), "photos"), // +3h, whole-hour offset
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        let rows = group_rows(&conn, ws);
        assert_eq!(rows, vec![(45.0, "parent+mtime".to_string())]);
    }

    #[test]
    fn below_tier_e_forms_no_group() {
        // Different name, different parent, no mtime signal at all.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), "alpha"),
            ("b.jpg", 500_000, Some("2026-06-15_03:22:10"), "beta"),
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert!(group_rows(&conn, ws).is_empty());
    }

    #[test]
    fn clique_requirement_rejects_transitive_chain() {
        // A and B: exact name + strong mtime (tier C).
        // B and C: same parent folder + moderate mtime, different names (tier E).
        // A and C: no relationship at all (different name, different parent,
        // no mtime support) -- union-find would have merged all three into
        // one component via the A-B and B-C edges; the clique model must not.
        let mut conn = setup_files(&[
            ("shared.jpg", 500_000, Some("2024-01-01_10:00:00"), "alpha"), // A
            ("shared.jpg", 500_000, Some("2024-01-01_10:00:01"), "beta"),  // B
            ("other.jpg", 500_000, Some("2024-01-01_13:00:01"), "beta"),   // C
        ]);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();

        let mut rows = group_rows(&conn, ws);
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        assert_eq!(
            rows,
            vec![
                (70.0, "name+mtime".to_string()),
                (45.0, "parent+mtime".to_string())
            ],
            "must form two separate groups (tier C for A~B, tier E for B~C), never one merged group"
        );

        // A and C must never co-occur in the same group.
        let a_group: i64 = conn
            .query_row(
                "SELECT mm.group_id FROM match_members mm JOIN nodes n ON n.id = mm.node_id
                 WHERE n.name = 'shared.jpg' AND n.parent_id IN (SELECT id FROM nodes WHERE name = 'alpha')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let c_group: i64 = conn
            .query_row(
                "SELECT mm.group_id FROM match_members mm JOIN nodes n ON n.id = mm.node_id
                 WHERE n.name = 'other.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(a_group, c_group);
    }

    #[test]
    fn mtime_two_second_tolerance_is_strong() {
        assert_eq!(
            compare_mtime(Some("2024-01-01_10:00:00"), Some("2024-01-01_10:00:02")),
            MtimeMatch::Strong
        );
    }

    #[test]
    fn mtime_whole_hour_offset_is_moderate() {
        assert_eq!(
            compare_mtime(Some("2024-01-01_10:00:00"), Some("2024-01-01_11:00:00")),
            MtimeMatch::Moderate
        );
    }

    #[test]
    fn mtime_reset_is_not_a_veto_only_a_downgrade() {
        assert_eq!(
            compare_mtime(Some("2024-01-01_10:00:00"), Some("2026-08-05_00:00:00")),
            MtimeMatch::None
        );
    }

    #[test]
    fn oversized_metadata_group_is_dropped_not_persisted() {
        let mut rows: Vec<(String, i64, Option<String>, String)> = Vec::new();
        for i in 0..(MAX_GROUP_SIZE + 1) {
            rows.push((
                "clone.bin".to_string(),
                42_000,
                Some(format!("2024-01-01_10:{:02}:00", i % 60)),
                String::new(),
            ));
        }
        let borrowed: Vec<(&str, i64, Option<&str>, &str)> = rows
            .iter()
            .map(|(n, s, m, p)| (n.as_str(), *s, m.as_deref(), p.as_str()))
            .collect();
        let mut conn = setup_files(&borrowed);
        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert!(
            group_rows(&conn, ws).is_empty(),
            "a {}-member name-group must be dropped, not persisted",
            MAX_GROUP_SIZE + 1
        );
    }
}
