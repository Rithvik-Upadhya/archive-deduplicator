//! The duplicate-matching engine. Because we never have file contents, matching
//! relies entirely on metadata. The primary signal is **exact file size**
//! (catches renames); name, mtime, parent-folder and inode/dev are supporting
//! signals folded into a 0-100 confidence score. Files below a tunable size are
//! deprioritized to cut UI noise.

use rusqlite::{params, Connection};
use std::collections::HashMap;

/// A file node loaded for matching.
struct FileRow {
    id: i64,
    source_id: i64,
    parent_id: Option<i64>,
    name: String,
    size: i64,
    mtime: Option<String>,
    inode: Option<i64>,
    dev: Option<i64>,
    parent_name: String,
}

/// Tunable parameters supplied from the UI.
#[derive(Clone, Copy)]
pub struct DedupParams {
    /// Files strictly smaller than this many bytes are excluded from matching
    /// entirely (hard filter — they generate size-collision noise).
    pub min_size_bytes: i64,
    /// Groups below this confidence are not persisted.
    pub min_confidence: f64,
}

impl Default for DedupParams {
    fn default() -> Self {
        DedupParams {
            min_size_bytes: 4096,
            min_confidence: 40.0,
        }
    }
}

/// Simple union-find for grouping matched files into connected components.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// Compare the file stem (name without final extension).
fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Compare the extension (lowercased) of two names.
fn ext(name: &str) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => name[i + 1..].to_lowercase(),
        _ => String::new(),
    }
}

/// Score a candidate pair of same-size files. Returns (confidence 0-100, primary signal).
/// Both files share the same size (they are in the same bucket).
fn score_pair(a: &FileRow, b: &FileRow, params: &DedupParams) -> (f64, &'static str) {
    let mut score: f64 = 30.0; // baseline for identical size
    let mut primary = "size";

    let name_eq = a.name.eq_ignore_ascii_case(&b.name);
    if a.name == b.name {
        score += 35.0;
        primary = "name+size";
    } else if name_eq {
        score += 25.0;
        primary = "name+size";
    } else if stem(&a.name).eq_ignore_ascii_case(stem(&b.name)) {
        score += 15.0;
    } else if !ext(&a.name).is_empty() && ext(&a.name) == ext(&b.name) {
        score += 5.0;
    }

    // Modification time.
    match (&a.mtime, &b.mtime) {
        (Some(x), Some(y)) if x == y => score += 20.0,
        (Some(x), Some(y)) if same_minute(x, y) => score += 10.0,
        _ => {}
    }

    // Parent folder name.
    if !a.parent_name.is_empty() && a.parent_name.eq_ignore_ascii_case(&b.parent_name) {
        score += 10.0;
    }

    // inode + dev equality is only meaningful within the same source (hardlink /
    // identical physical file). Across different sources it is coincidental.
    if a.source_id == b.source_id {
        if let (Some(ia), Some(ib), Some(da), Some(db)) = (a.inode, b.inode, a.dev, b.dev) {
            if ia == ib && da == db {
                score += 25.0;
                primary = "inode";
            }
        }
    }

    let _ = params; // size filtering happens before pairing now
                    // Zero-byte files are almost pure noise.
    if a.size == 0 {
        score -= 20.0;
    }

    (score.clamp(0.0, 100.0), primary)
}

/// Compare the "YYYY-MM-DD_HH:MM" prefix of two `tree`-format timestamps.
fn same_minute(a: &str, b: &str) -> bool {
    let la = a.len().min(16);
    let lb = b.len().min(16);
    la == lb && a[..la] == b[..lb]
}

/// Run the full dedup pass for a workspace: clears prior groups, rebuilds file
/// groups, then folder-level groups, all inside one transaction.
pub fn run(
    conn: &mut Connection,
    workspace_id: i64,
    params: DedupParams,
) -> rusqlite::Result<usize> {
    // Load parent-name lookup and all files for the workspace.
    let parent_names = load_parent_names(conn, workspace_id)?;
    let files = load_files(conn, workspace_id, &parent_names)?;

    // Bucket file indices by size. Files below the tunable minimum are
    // excluded entirely: tiny files collide by size constantly and only
    // generate noise, so the slider acts as a hard filter.
    let mut by_size: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size < params.min_size_bytes {
            continue;
        }
        by_size.entry(f.size).or_default().push(i);
    }

    let mut uf = UnionFind::new(files.len());
    // Confidence of the strongest edge each node participated in.
    let mut best_edge: Vec<f64> = vec![0.0; files.len()];
    let mut best_signal: Vec<&'static str> = vec!["size"; files.len()];

    for (_size, idxs) in by_size.iter() {
        if idxs.len() < 2 {
            continue;
        }
        // Cap bucket comparisons to avoid quadratic blowups on pathological sizes.
        let n = idxs.len();
        for a in 0..n {
            for b in (a + 1)..n {
                let ia = idxs[a];
                let ib = idxs[b];
                let (conf, sig) = score_pair(&files[ia], &files[ib], &params);
                if conf >= params.min_confidence {
                    uf.union(ia, ib);
                    if conf > best_edge[ia] {
                        best_edge[ia] = conf;
                        best_signal[ia] = sig;
                    }
                    if conf > best_edge[ib] {
                        best_edge[ib] = conf;
                        best_signal[ib] = sig;
                    }
                }
            }
        }
    }

    // Collect components with >= 2 members.
    let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..files.len() {
        if best_edge[i] > 0.0 {
            let root = uf.find(i);
            components.entry(root).or_default().push(i);
        }
    }

    let tx = conn.transaction()?;
    // Clear previous results for this workspace.
    tx.execute(
        "DELETE FROM match_groups WHERE workspace_id = ?1",
        params![workspace_id],
    )?;

    let mut group_count = 0usize;
    for (_root, members) in components.iter() {
        if members.len() < 2 {
            continue;
        }
        // Only keep a group if it spans real duplicates (same size guaranteed).
        let avg_conf = members.iter().map(|&i| best_edge[i]).sum::<f64>() / members.len() as f64;
        let size = files[members[0]].size;
        let signal = best_signal[members[0]];

        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', ?2, ?3, ?4)",
            params![workspace_id, avg_conf, signal, size],
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
    let folder_groups = super::rollup::build_folder_groups(conn, workspace_id)?;

    // Drop groups left with fewer than two members. Deleting a source cascades
    // its `match_members` rows away but leaves the parent group behind, so a
    // stale one-member "group" would otherwise linger in the review list.
    conn.execute(
        "DELETE FROM match_groups
         WHERE workspace_id = ?1
           AND (SELECT COUNT(*) FROM match_members mm WHERE mm.group_id = match_groups.id) < 2",
        params![workspace_id],
    )?;

    // Rebuild the per-node duplicate annotation cache so tree browsing is fast.
    super::rollup::rebuild_annotations(conn, workspace_id)?;

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
         WHERE s.workspace_id = ?1 AND n.type = 'directory'",
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
fn load_files(
    conn: &Connection,
    workspace_id: i64,
    parent_names: &HashMap<i64, String>,
) -> rusqlite::Result<Vec<FileRow>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.size, n.mtime, n.inode, n.dev
         FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'file'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        let parent_id: Option<i64> = r.get(2)?;
        Ok(FileRow {
            id: r.get(0)?,
            source_id: r.get(1)?,
            parent_id,
            name: r.get(3)?,
            size: r.get(4)?,
            mtime: r.get(5)?,
            inode: r.get(6)?,
            dev: r.get(7)?,
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

    #[test]
    fn detects_cross_source_file_duplicates() {
        let mut conn = setup_two_identical_sources();
        let ws = 1;
        let count = run(&mut conn, ws, DedupParams::default()).unwrap();
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
        run(&mut conn, ws, params).unwrap();
        let file_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND kind = 'file'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(file_groups, 0, "tiny files should be deprioritized out");
    }
}
