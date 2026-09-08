//! Folder-level duplicate detection and device statistics. Built on top of the
//! file-level match groups produced by `dedup.rs`.
//!
//! A folder is considered a duplicate candidate of another folder when a high
//! fraction of its files (by count *and* by size) match files inside the other
//! folder. We approximate this by asking: for each directory, what fraction of
//! its subtree bytes belong to files that are matched to files in some *other*
//! source. Directories that are almost entirely duplicated are then clustered
//! with the folders they overlap with.

use rusqlite::{Connection, params};
use std::collections::{HashMap, HashSet};

/// A file's location used for folder rollups.
pub(crate) struct FileLoc {
    pub(crate) node_id: i64,
    pub(crate) source_id: i64,
    pub(crate) parent_id: Option<i64>,
    pub(crate) size: i64,
}

/// Build folder-level match groups. Returns the number of folder groups written.
///
/// Thin wrapper that loads `files`/`parent_of` itself, kept only for tests
/// that exercise this in isolation. `dedup::run_with_progress` -- the only
/// production caller -- uses `build_folder_groups_with` directly instead,
/// since it and `rebuild_annotations` both need the exact same two
/// full-table loads and are always called back-to-back in the same pass;
/// loading twice there would be pure duplicated I/O.
#[cfg(test)]
pub(crate) fn build_folder_groups(
    conn: &mut Connection,
    workspace_id: i64,
) -> rusqlite::Result<usize> {
    let files = load_file_locs(conn, workspace_id)?;
    let parent_of = load_parent_of(conn, workspace_id)?;
    build_folder_groups_with(conn, workspace_id, &files, &parent_of)
}

pub(crate) fn build_folder_groups_with(
    conn: &mut Connection,
    workspace_id: i64,
    files: &[FileLoc],
    parent_of: &HashMap<i64, Option<i64>>,
) -> rusqlite::Result<usize> {
    // node_id -> group_id for file members.
    let mut file_group: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT mm.node_id, mm.group_id FROM match_members mm
             JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (nid, gid) = row?;
            file_group.insert(nid, gid);
        }
    }

    let source_of = load_source_of(conn, workspace_id)?;

    // For each directory: total subtree file bytes, and duplicated bytes (files
    // whose match group also contains a file from a different source), plus the
    // set of "partner" directories those matched files live in.
    let mut dir_total: HashMap<i64, i64> = HashMap::new();
    let mut dir_dup: HashMap<i64, i64> = HashMap::new();

    // group_id -> list of (source_id, parent_dir_id) to detect cross-source dup.
    let mut group_sources: HashMap<i64, HashSet<i64>> = HashMap::new();
    for f in files {
        if let Some(gid) = file_group.get(&f.node_id) {
            group_sources.entry(*gid).or_default().insert(f.source_id);
        }
    }

    for f in files {
        // Accumulate this file's size into every ancestor directory's total.
        let mut cur = f.parent_id;
        while let Some(dir) = cur {
            *dir_total.entry(dir).or_insert(0) += f.size;
            cur = parent_of.get(&dir).copied().flatten();
        }
        // Is this file duplicated across sources?
        let is_cross_dup = file_group
            .get(&f.node_id)
            .and_then(|gid| group_sources.get(gid))
            .map(|srcs| srcs.len() > 1)
            .unwrap_or(false);
        if is_cross_dup {
            let mut cur = f.parent_id;
            while let Some(dir) = cur {
                *dir_dup.entry(dir).or_insert(0) += f.size;
                cur = parent_of.get(&dir).copied().flatten();
            }
        }
    }

    // A directory qualifies as a "duplicated folder" when >= 80% of its subtree
    // bytes are cross-source duplicated and it holds a meaningful amount of data.
    let mut dup_dirs: Vec<(i64, f64, i64)> = Vec::new(); // (dir_id, pct, total)
    for (dir, total) in &dir_total {
        if *total <= 0 {
            continue;
        }
        let dup = *dir_dup.get(dir).unwrap_or(&0);
        let pct = dup as f64 / *total as f64 * 100.0;
        if pct >= 80.0 {
            dup_dirs.push((*dir, pct, *total));
        }
    }

    // Cluster duplicated directories that share the same basename across
    // different sources into folder groups.
    let dir_names = load_dir_names(conn, workspace_id)?;
    let mut by_name: HashMap<String, Vec<(i64, f64, i64)>> = HashMap::new();
    for (dir, pct, total) in dup_dirs {
        // Skip a directory if its own parent is also a fully-duplicated dir, to
        // report duplication at the highest folder level rather than every level.
        if let Some(Some(parent)) = parent_of.get(&dir).copied() {
            if dir_total.contains_key(&parent) {
                let ptotal = dir_total[&parent];
                let pdup = *dir_dup.get(&parent).unwrap_or(&0);
                if ptotal > 0 && (pdup as f64 / ptotal as f64) >= 0.8 {
                    continue;
                }
            }
        }
        if let Some(name) = dir_names.get(&dir) {
            by_name
                .entry(name.to_lowercase())
                .or_default()
                .push((dir, pct, total));
        }
    }

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for (_name, mut dirs) in by_name {
        // Need the same-named folder present in at least two different sources.
        let distinct_sources: HashSet<i64> = dirs
            .iter()
            .filter_map(|(d, _, _)| source_of.get(d).copied())
            .collect();
        if dirs.len() < 2 || distinct_sources.len() < 2 {
            continue;
        }
        dirs.sort_by(|a, b| b.2.cmp(&a.2));
        let avg_pct = dirs.iter().map(|d| d.1).sum::<f64>() / dirs.len() as f64;
        let max_total = dirs.iter().map(|d| d.2).max().unwrap_or(0);

        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'folder', ?2, 'folder', ?3)",
            params![workspace_id, avg_pct, max_total],
        )?;
        let gid = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            )?;
            for (dir, _, _) in &dirs {
                stmt.execute(params![gid, dir])?;
            }
        }
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}

/// Truncate a `tree`-format mtime to an even 2-second boundary (FAT/exFAT's
/// own granularity) before folding it into a structural hash. Unlike
/// `dedup.rs::compare_mtime`'s tolerant *comparison*, this bakes a value into
/// a hash, so only the one noise source safe to fully absorb (2s rounding)
/// is normalized here -- a whole-hour timezone offset is deliberately left
/// un-normalized, since folding an hour of slack into a byte-for-byte
/// structural fingerprint would risk two genuinely different-vintage
/// folders colliding. `None`/unparsable mtimes get a fixed sentinel so they
/// still contribute a stable (if uninformative) value rather than being
/// silently skipped.
fn mtime_norm_secs(mtime: Option<&str>) -> i64 {
    let Some(m) = mtime else {
        return i64::MIN;
    };
    let Some(dt) = chrono::NaiveDateTime::parse_from_str(m, "%Y-%m-%d_%H:%M:%S").ok() else {
        return i64::MIN + 1;
    };
    let secs = dt.and_utc().timestamp();
    secs - secs.rem_euclid(2)
}

/// Confidence assigned to a listing-hash folder match: structural-only
/// evidence (no byte content verified), stronger than a bare `dup_pct`
/// heuristic but never "Confirmed" -- placed between tier D (55) and tier C
/// (70) in `dedup.rs`'s metadata scale.
const LISTING_HASH_CONFIDENCE: f64 = 60.0;

/// Compute a Merkle-style structural fingerprint for every directory in the
/// workspace, then cluster directories sharing an identical fingerprint
/// across >= 2 distinct sources into `kind='folder', primary_signal='listing'`
/// groups. Functionally equivalent to hashing the sorted
/// `(rel_name, size, mtime_normalised)` tuple list over a directory's entire
/// subtree (the design spec's own framing), but computed bottom-up in
/// `O(total nodes)`: each directory's fingerprint folds in its immediate
/// children only, using subdirectories' *already-computed* fingerprints
/// rather than re-serializing every descendant at every ancestor level.
///
/// This catches two things the byte-overlap heuristic in
/// `build_folder_groups` misses: directories dominated by many small
/// (never-hashed) files, and renamed-but-otherwise-identical folders (the
/// heuristic requires matching basenames; this doesn't care what a folder
/// is named, only what it contains).
pub fn compute_listing_hashes(conn: &mut Connection, workspace_id: i64) -> rusqlite::Result<usize> {
    struct DirNode {
        id: i64,
        parent_id: Option<i64>,
        name: String,
        depth: i64,
    }
    let dirs: Vec<DirNode> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.parent_id, n.name, n.depth FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
        )?;
        stmt.query_map(params![workspace_id], |r| {
            Ok(DirNode {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                name: r.get(2)?,
                depth: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut files_by_parent: HashMap<i64, Vec<(String, i64, Option<String>)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT n.parent_id, n.name, n.size, n.mtime FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.type = 'file' AND n.alias_of IS NULL AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (parent_id, name, size, mtime) = row?;
            if let Some(pid) = parent_id {
                files_by_parent
                    .entry(pid)
                    .or_default()
                    .push((name, size, mtime));
            }
        }
    }

    // Symlinks fold into the fingerprint too: a directory containing one
    // must not hash identically to one without, or two structurally
    // different folders would cluster as a false positive -- the one place
    // this app is least tolerant of them. `link_target` is NULL until Stage 4
    // populates it, but even NULL still distinguishes "has a link named X"
    // from "has no link", which is the part that matters here.
    let mut links_by_parent: HashMap<i64, Vec<(String, Option<String>)>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT n.parent_id, n.name, n.link_target FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.type = 'link' AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (parent_id, name, link_target) = row?;
            if let Some(pid) = parent_id {
                links_by_parent
                    .entry(pid)
                    .or_default()
                    .push((name, link_target));
            }
        }
    }

    let mut children_dirs: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, d) in dirs.iter().enumerate() {
        if let Some(pid) = d.parent_id {
            children_dirs.entry(pid).or_default().push(i);
        }
    }

    let mut order: Vec<usize> = (0..dirs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(dirs[i].depth));

    let mut listing_hash: Vec<[u8; 32]> = vec![[0u8; 32]; dirs.len()];
    let mut nonempty: Vec<bool> = vec![false; dirs.len()];
    for i in order {
        let dir_id = dirs[i].id;
        // (sort key, hash-input bytes) per immediate child, so the final
        // hash is independent of database iteration order.
        let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
        if let Some(files) = files_by_parent.get(&dir_id) {
            for (name, size, mtime) in files {
                let mut bytes = Vec::with_capacity(name.len() + 24);
                bytes.extend_from_slice(b"file:");
                bytes.extend_from_slice(name.as_bytes());
                bytes.extend_from_slice(&size.to_le_bytes());
                bytes.extend_from_slice(&mtime_norm_secs(mtime.as_deref()).to_le_bytes());
                entries.push((name.clone(), bytes));
            }
        }
        if let Some(links) = links_by_parent.get(&dir_id) {
            for (name, link_target) in links {
                let mut bytes = Vec::with_capacity(name.len() + 16);
                bytes.extend_from_slice(b"link:");
                bytes.extend_from_slice(name.as_bytes());
                bytes.extend_from_slice(link_target.as_deref().unwrap_or_default().as_bytes());
                entries.push((name.clone(), bytes));
            }
        }
        if let Some(child_idxs) = children_dirs.get(&dir_id) {
            for &ci in child_idxs {
                let mut bytes = Vec::with_capacity(dirs[ci].name.len() + 37);
                bytes.extend_from_slice(b"dir:");
                bytes.extend_from_slice(dirs[ci].name.as_bytes());
                bytes.extend_from_slice(&listing_hash[ci]);
                entries.push((dirs[ci].name.clone(), bytes));
            }
        }
        nonempty[i] = !entries.is_empty();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut hasher = blake3::Hasher::new();
        for (_, bytes) in &entries {
            hasher.update(bytes);
            hasher.update(b"\n");
        }
        listing_hash[i] = *hasher.finalize().as_bytes();
    }

    let tx = conn.transaction()?;
    {
        // One `UPDATE` per directory inside a single transaction with a
        // statement prepared once (see `links.rs::recompute_subtree_totals`'s
        // matching note) -- chunking into savepoints or a temp-table
        // `UPDATE ... FROM` would add real complexity for a gain that
        // hasn't been measured to exist on top of that.
        let mut stmt = tx.prepare("UPDATE nodes SET listing_hash = ?1 WHERE id = ?2")?;
        for (i, d) in dirs.iter().enumerate() {
            stmt.execute(params![listing_hash[i].to_vec(), d.id])?;
        }
    }

    let source_of = load_source_of(&tx, workspace_id)?;
    let mut by_hash: HashMap<[u8; 32], Vec<i64>> = HashMap::new();
    for (i, d) in dirs.iter().enumerate() {
        // An empty (or all-empty-descendant) directory hashes identically to
        // every other empty directory in the workspace -- that tells us
        // nothing structurally, so it must never seed a cluster.
        if !nonempty[i] {
            continue;
        }
        by_hash.entry(listing_hash[i]).or_default().push(d.id);
    }

    let dir_parent: HashMap<i64, Option<i64>> = dirs.iter().map(|d| (d.id, d.parent_id)).collect();

    // A parent directory's fingerprint folds in its children's fingerprints,
    // so a matched root also makes every directory beneath it match -- that
    // would otherwise emit a listing group at every level of an identical
    // subtree. `clustered` holds every dir id that qualifies for a group (>=2
    // members, >=2 distinct sources) so the emission pass below can skip a
    // dir whose parent is *also* clustered, keeping only the highest
    // (root) matching level.
    let mut clustered: HashMap<i64, [u8; 32]> = HashMap::new();
    for (&hash, dir_ids) in &by_hash {
        if dir_ids.len() < 2 {
            continue;
        }
        let distinct_sources: HashSet<i64> = dir_ids
            .iter()
            .filter_map(|d| source_of.get(d).copied())
            .collect();
        if distinct_sources.len() < 2 {
            continue;
        }
        for &did in dir_ids {
            clustered.insert(did, hash);
        }
    }

    let mut count = 0usize;
    for dir_ids in by_hash.into_values() {
        if dir_ids.len() < 2 {
            continue;
        }
        // Emit only cluster roots: drop any member whose parent is also
        // clustered, since that parent's group already covers this level.
        let root_ids: Vec<i64> = dir_ids
            .iter()
            .filter(|did| {
                !matches!(dir_parent.get(did), Some(Some(pid)) if clustered.contains_key(pid))
            })
            .copied()
            .collect();
        if root_ids.len() < 2 {
            continue;
        }
        let distinct_sources: HashSet<i64> = root_ids
            .iter()
            .filter_map(|d| source_of.get(d).copied())
            .collect();
        if distinct_sources.len() < 2 {
            continue;
        }
        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'folder', ?2, 'listing', 0)",
            params![workspace_id, LISTING_HASH_CONFIDENCE],
        )?;
        let gid = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            )?;
            for did in &root_ids {
                stmt.execute(params![gid, did])?;
            }
        }
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}

pub(crate) fn load_file_locs(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<Vec<FileLoc>> {
    // `s.excluded = 0` mirrors `dedup.rs::load_files`: an excluded source is out
    // of the matcher entirely, so its files must not feed folder rollups or the
    // `dup_annot` cache either.
    let mut stmt = conn.prepare(
        "SELECT n.id, n.source_id, n.parent_id, n.size FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'file' AND n.alias_of IS NULL AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(FileLoc {
            node_id: r.get(0)?,
            source_id: r.get(1)?,
            parent_id: r.get(2)?,
            size: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// A file or symlink used for directory-level leaf-count rollups. Distinct
/// from `FileLoc`, which is byte-weighted and file-only -- this counts every
/// leaf (files *and* symlinks) so a directory containing a symlink is never
/// mistaken for "fully duplicated" just because all its *files* are.
struct LeafLoc {
    node_id: i64,
    parent_id: Option<i64>,
    is_file: bool,
}

fn load_leaf_locs(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<LeafLoc>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id, n.type FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type IN ('file', 'link') AND n.alias_of IS NULL AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(LeafLoc {
            node_id: r.get(0)?,
            parent_id: r.get(1)?,
            is_file: r.get::<_, String>(2)? == "file",
        })
    })?;
    rows.collect()
}

pub(crate) fn load_parent_of(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, Option<i64>>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, parent) = row?;
        map.insert(id, parent);
    }
    Ok(map)
}

fn load_source_of(conn: &Connection, workspace_id: i64) -> rusqlite::Result<HashMap<i64, i64>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.source_id FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, src) = row?;
        map.insert(id, src);
    }
    Ok(map)
}

fn load_dir_names(conn: &Connection, workspace_id: i64) -> rusqlite::Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        map.insert(id, name);
    }
    Ok(map)
}

/// Compute duplicated bytes per source. A file's bytes count as duplicated when
/// its match group spans more than one source (every copy is redundant
/// somewhere else), **or** when the group is internal to a single source, in
/// which case all but one copy per source are surplus (`(k - 1) * size`).
///
/// The result is measured against a source's *physical* bytes, not its logical
/// `total_size`: hardlink aliases are excluded from the matcher's candidate
/// pool (`dedup.rs::load_files` filters `alias_of IS NULL`), so alias bytes can
/// never appear here. `source_list` divides by `sources.physical_size` to
/// match -- dividing by `total_size` caps the ratio at `physical/total`, which
/// on an alias-heavy device reads as a low percentage that no amount of real
/// duplication can lift.
pub fn duplicated_size_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, i64>> {
    // group_id -> set of source ids and list of (group, node, source, size).
    let mut group_srcs: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut members: Vec<(i64, i64, i64, i64)> = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT mm.group_id, mm.node_id, n.source_id, n.size FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (gid, node_id, src, size) = row?;
        group_srcs.entry(gid).or_default().insert(src);
        members.push((gid, node_id, src, size));
    }

    // For internal-only groups we count all-but-one copy per source, so we
    // track (group, source) -> member count seen so far.
    let mut seen: HashMap<(i64, i64), i64> = HashMap::new();
    // As in `cross_dup_size_by_source` below: a file may sit in more than one
    // match group across tiers (see dedup.rs's note on mixed groups), so count
    // each node's bytes at most once per source. Without this the total can
    // exceed the source's own physical size and the badge prints over 100%.
    let mut counted: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut result: HashMap<i64, i64> = HashMap::new();
    for (gid, node_id, src, size) in members {
        let cross = group_srcs.get(&gid).map(|s| s.len() > 1).unwrap_or(false);
        let surplus = if cross {
            true
        } else {
            // Internal duplicates: first copy is the "original", the rest count.
            // This counter is per (group, source) and stays independent of the
            // per-node guard -- it decides *whether* a copy is surplus, while
            // `counted` decides whether an already-surplus node has been
            // billed to this source under some other group.
            let count = seen.entry((gid, src)).or_insert(0);
            let is_surplus = *count > 0;
            *count += 1;
            is_surplus
        };
        if surplus && counted.entry(src).or_default().insert(node_id) {
            *result.entry(src).or_insert(0) += size;
        }
    }
    Ok(result)
}

/// Cross-device duplicate bytes and file count per source. Unlike
/// `duplicated_size_by_source` (which pools cross-source and internal-only
/// duplication together for the "% dup" badges), this counts only bytes/files
/// whose match group spans more than one source -- exactly what the
/// "exclusive to this device" filter hides at the device-header level.
pub fn cross_dup_size_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, (i64, i64)>> {
    let mut group_srcs: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut members: Vec<(i64, i64, i64, i64)> = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT mm.group_id, mm.node_id, n.source_id, n.size FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (gid, node_id, src, size) = row?;
        group_srcs.entry(gid).or_default().insert(src);
        members.push((gid, node_id, src, size));
    }

    // A file may legitimately sit in more than one match group across tiers
    // (see dedup.rs's note on mixed groups); count each node's bytes at most
    // once per source, even if several of its groups are cross-device.
    let mut counted: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut result: HashMap<i64, (i64, i64)> = HashMap::new();
    for (gid, node_id, src, size) in members {
        let cross = group_srcs.get(&gid).map(|s| s.len() > 1).unwrap_or(false);
        if cross && counted.entry(src).or_default().insert(node_id) {
            let entry = result.entry(src).or_insert((0, 0));
            entry.0 += size;
            entry.1 += 1;
        }
    }
    Ok(result)
}

/// Rebuild the `dup_annot` cache: per-file duplicate flags and per-directory
/// duplicated-percentage rollups. Called once after each dedup run so that
/// `get_tree` can serve annotations with a simple JOIN instead of recomputing
/// the whole workspace on every expand.
///
/// `cross_dup` backs the per-device "exclusive to this device" filter: for a
/// file it means "a duplicate of this file exists on a different device
/// (source_id)"; for a directory it means "every leaf (file or symlink) in
/// this subtree is such a file" -- i.e. nothing would remain visible in this
/// subtree if cross-device duplicates were filtered out. `cross_dup_size` /
/// `cross_dup_file_count` (directories only) are the byte size and file count
/// of that hidden portion, so the UI can show the filtered file count/size
/// without re-walking the tree client-side. `in_folder_group` (directories
/// only) is whether the directory is a genuine member of a clustered
/// folder-kind match group, as opposed to merely having `dup_pct > 0`.
///
/// Thin wrapper loading `files`/`parent_of` itself, kept only for tests that
/// exercise this in isolation; see `build_folder_groups`'s doc comment --
/// `dedup::run_with_progress` calls `rebuild_annotations_with` directly
/// instead, reusing the same loads `build_folder_groups_with` made moments
/// earlier in the same pass.
#[cfg(test)]
pub(crate) fn rebuild_annotations(
    conn: &mut Connection,
    workspace_id: i64,
) -> rusqlite::Result<()> {
    let files = load_file_locs(conn, workspace_id)?;
    let parent_of = load_parent_of(conn, workspace_id)?;
    rebuild_annotations_with(conn, workspace_id, &files, &parent_of)
}

pub(crate) fn rebuild_annotations_with(
    conn: &mut Connection,
    workspace_id: i64,
    files: &[FileLoc],
    parent_of: &HashMap<i64, Option<i64>>,
) -> rusqlite::Result<()> {
    // A file is a duplicate when its group has >= 2 members anywhere (cross-
    // source or internal).
    let mut dup_files: HashSet<i64> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT mm.node_id FROM match_members mm
             JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mg.workspace_id = ?1 AND mg.kind = 'file'
               AND (SELECT COUNT(*) FROM match_members m2 WHERE m2.group_id = mm.group_id) >= 2",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| r.get::<_, i64>(0))?;
        for row in rows {
            dup_files.insert(row?);
        }
    }

    // Hardlink aliases are absent from `files` (`load_file_locs` filters
    // `alias_of IS NULL`) and so never reach the matcher, which means without
    // this they get no `dup_annot` row at all -- and every consumer that reads
    // `COALESCE(cross_dup, 0)` then treats them as exclusive-to-this-device.
    // That is how a filtered drag used to carry across a second *name* for
    // content whose canonical the very same filter had just hidden.
    //
    // An alias is another name for exactly the canonical's bytes, so it
    // inherits the canonical's duplicate state wholesale. Note what it does
    // *not* inherit: bytes. An alias is a name with zero marginal size, so it
    // is added to every count below and to no byte total.
    let aliases: Vec<(i64, i64, Option<i64>)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.alias_of, n.parent_id FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.alias_of IS NOT NULL AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (alias_id, canonical_id, _) in &aliases {
        if dup_files.contains(canonical_id) {
            dup_files.insert(*alias_id);
        }
    }

    // Roll duplicated bytes up the ancestor chain.
    let mut dir_dup: HashMap<i64, i64> = HashMap::new();
    for f in files {
        if !dup_files.contains(&f.node_id) {
            continue;
        }
        let mut cur = f.parent_id;
        while let Some(dir) = cur {
            *dir_dup.entry(dir).or_insert(0) += f.size;
            cur = parent_of.get(&dir).copied().flatten();
        }
    }

    // A file is a *cross-device* duplicate when its match group contains a
    // member from a different source_id (reuses the `group_sources` pattern
    // from `build_folder_groups`, restricted to file-kind groups so folder
    // groups' directory members don't leak into this check).
    let mut file_group: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT mm.node_id, mm.group_id FROM match_members mm
             JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (nid, gid) = row?;
            file_group.insert(nid, gid);
        }
    }
    let mut group_sources: HashMap<i64, HashSet<i64>> = HashMap::new();
    for f in files {
        if let Some(gid) = file_group.get(&f.node_id) {
            group_sources.entry(*gid).or_default().insert(f.source_id);
        }
    }
    let mut cross_dup_files: HashSet<i64> = files
        .iter()
        .filter(|f| {
            file_group
                .get(&f.node_id)
                .and_then(|gid| group_sources.get(gid))
                .map(|srcs| srcs.len() > 1)
                .unwrap_or(false)
        })
        .map(|f| f.node_id)
        .collect();
    for (alias_id, canonical_id, _) in &aliases {
        if cross_dup_files.contains(canonical_id) {
            cross_dup_files.insert(*alias_id);
        }
    }

    // Directory-level cross_dup is a leaf-*count* rollup (not byte-weighted
    // like dup_pct above): a directory is only fully cross-dup when every
    // name in its subtree -- file, symlink or hardlink alias -- is a cross_dup
    // file. Symlinks are never in cross_dup_files (dedup matching only loads
    // type='file' rows), so a directory holding even one symlink can never
    // read as fully cross-dup, keeping it visible under the filter. Aliases,
    // by contrast, now inherit their canonical's flag, so a directory holding
    // only hidden canonicals and their aliases correctly reads as fully
    // cross-dup and disappears whole.
    let leaves = load_leaf_locs(conn, workspace_id)?;
    let mut dir_leaf_total: HashMap<i64, i64> = HashMap::new();
    let mut dir_leaf_cross: HashMap<i64, i64> = HashMap::new();
    for leaf in &leaves {
        let mut cur = leaf.parent_id;
        while let Some(dir) = cur {
            *dir_leaf_total.entry(dir).or_insert(0) += 1;
            if leaf.is_file && cross_dup_files.contains(&leaf.node_id) {
                *dir_leaf_cross.entry(dir).or_insert(0) += 1;
            }
            cur = parent_of.get(&dir).copied().flatten();
        }
    }
    // `load_leaf_locs` filters aliases out, but a directory is only fully
    // hidden when every *name* under it is, so they are rolled up here on both
    // sides of the ratio -- otherwise a directory holding nothing but aliases
    // of cross-dup canonicals would read as having no leaves at all.
    for (alias_id, _, parent_id) in &aliases {
        let mut cur = *parent_id;
        while let Some(dir) = cur {
            *dir_leaf_total.entry(dir).or_insert(0) += 1;
            if cross_dup_files.contains(alias_id) {
                *dir_leaf_cross.entry(dir).or_insert(0) += 1;
            }
            cur = parent_of.get(&dir).copied().flatten();
        }
    }
    let dir_cross_dup = |dir: &i64| -> bool {
        let total = *dir_leaf_total.get(dir).unwrap_or(&0);
        total > 0 && dir_leaf_cross.get(dir).copied().unwrap_or(0) == total
    };

    // Byte size and file count of cross-dup files rolled up per ancestor
    // directory, so the UI can show "files exclusive to this device" counts
    // when the filter is on without re-walking the tree client-side. Every
    // directory that gets one of these is already visited by the `dir_dup`
    // loop below (cross_dup_files is a subset of dup_files), so no extra
    // dup_annot rows are needed for this.
    let mut dir_cross_size: HashMap<i64, i64> = HashMap::new();
    let mut dir_cross_count: HashMap<i64, i64> = HashMap::new();
    for f in files {
        if !cross_dup_files.contains(&f.node_id) {
            continue;
        }
        let mut cur = f.parent_id;
        while let Some(dir) = cur {
            *dir_cross_size.entry(dir).or_insert(0) += f.size;
            *dir_cross_count.entry(dir).or_insert(0) += 1;
            cur = parent_of.get(&dir).copied().flatten();
        }
    }
    // Aliases bump the *count* and not the size. This asymmetry is the whole
    // model, not an oversight: a count counts names, and hiding an alias
    // removes a name the user would otherwise have to deal with; a size counts
    // bytes actually held, and an alias holds none of its own. Concretely,
    // `subtree_size` excludes alias bytes as well, so adding them to
    // `cross_dup_size` would make `subtree_size - cross_dup_size` subtract
    // bytes that were never in the total. Do not "fix" this into symmetry.
    for (alias_id, _, parent_id) in &aliases {
        if !cross_dup_files.contains(alias_id) {
            continue;
        }
        let mut cur = *parent_id;
        while let Some(dir) = cur {
            *dir_cross_count.entry(dir).or_insert(0) += 1;
            cur = parent_of.get(&dir).copied().flatten();
        }
    }

    // Directories that are genuine members of a clustered folder-kind match
    // group (built by `build_folder_groups`, which already ran earlier in
    // the same dedup pass -- see `dedup::run`). Distinct from `dup_pct > 0`,
    // which just means *some* bytes underneath are duplicated somewhere;
    // this means the directory itself was clustered with a same-named
    // sibling on another device, i.e. there is an actual match group to
    // locate. Backs the "Locate duplicates" button gating on the frontend.
    let mut in_folder_group_dirs: HashSet<i64> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT mm.node_id FROM match_members mm
             JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mg.workspace_id = ?1 AND mg.kind = 'folder'",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| r.get::<_, i64>(0))?;
        for row in rows {
            in_folder_group_dirs.insert(row?);
        }
    }

    // Directory subtree sizes are precomputed on nodes.subtree_size.
    let mut dir_sizes: HashMap<i64, i64> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.subtree_size FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.type = 'directory' AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (id, sz) = row?;
            dir_sizes.insert(id, sz);
        }
    }

    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM dup_annot WHERE node_id IN (
            SELECT n.id FROM nodes n JOIN sources s ON s.id = n.source_id
            WHERE s.workspace_id = ?1)",
        params![workspace_id],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO dup_annot (node_id, has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count, in_folder_group)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for id in &dup_files {
            stmt.execute(params![
                id,
                1,
                0.0,
                cross_dup_files.contains(id) as i64,
                0,
                0,
                0
            ])?;
        }
        for (dir, dup) in &dir_dup {
            let total = *dir_sizes.get(dir).unwrap_or(&0);
            let pct = if total > 0 {
                *dup as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            stmt.execute(params![
                dir,
                (pct > 0.0) as i64,
                pct,
                dir_cross_dup(dir) as i64,
                dir_cross_size.get(dir).copied().unwrap_or(0),
                dir_cross_count.get(dir).copied().unwrap_or(0),
                in_folder_group_dirs.contains(dir) as i64,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Build an in-memory workspace with two sources. Returns (conn,
    /// workspace_id, source_a_id, source_b_id).
    fn setup() -> (Connection, i64, i64, i64) {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        let mut src_ids = Vec::new();
        for label in ["device-a", "device-b"] {
            conn.execute(
                "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
                 VALUES (?1, 'scan', ?2, ?2, 't', 0, 0)",
                params![ws, label],
            )
            .unwrap();
            src_ids.push(conn.last_insert_rowid());
        }
        (conn, ws, src_ids[0], src_ids[1])
    }

    fn insert_node(
        conn: &Connection,
        source_id: i64,
        parent_id: Option<i64>,
        name: &str,
        node_type: &str,
        size: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, subtree_size, subtree_file_count)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?5, 1)",
            params![source_id, parent_id, name, node_type, size],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_file_group(conn: &Connection, ws: i64, member_ids: &[i64]) {
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', 100.0, 'test', 0)",
            params![ws],
        )
        .unwrap();
        let gid = conn.last_insert_rowid();
        for nid in member_ids {
            conn.execute(
                "INSERT INTO match_members (group_id, node_id) VALUES (?1, ?2)",
                params![gid, nid],
            )
            .unwrap();
        }
    }

    /// (has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count) for
    /// a node, or None if unannotated.
    fn annot(conn: &Connection, node_id: i64) -> Option<(i64, f64, i64, i64, i64)> {
        conn.query_row(
            "SELECT has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count
             FROM dup_annot WHERE node_id = ?1",
            params![node_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok()
    }

    /// `cross_dup` as `get_tree` would serve it: a missing row (a directory
    /// with no duplicated content at all never gets one) defaults to 0,
    /// mirroring the `COALESCE(d.cross_dup, 0)` in the get_tree SQL.
    fn cross_dup_of(conn: &Connection, node_id: i64) -> i64 {
        annot(conn, node_id).map(|a| a.2).unwrap_or(0)
    }

    /// `in_folder_group` as `get_tree` would serve it (see `cross_dup_of`).
    fn in_folder_group_of(conn: &Connection, node_id: i64) -> i64 {
        conn.query_row(
            "SELECT in_folder_group FROM dup_annot WHERE node_id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    }

    #[test]
    fn file_duplicated_across_sources_is_cross_dup() {
        let (mut conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "photo.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "photo.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);

        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(annot(&conn, a).unwrap().2, 1);
        assert_eq!(annot(&conn, b).unwrap().2, 1);
    }

    #[test]
    fn excluding_a_sources_only_partner_clears_cross_dup_on_the_other() {
        // Simulates a rerun after `dedup::run` stops loading an excluded
        // source's nodes: its match group no longer exists, so its former
        // partner's file must stop reading as cross_dup (and reappear under
        // the "exclusive to this device" filter).
        let (mut conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "photo.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "photo.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);
        rebuild_annotations(&mut conn, ws).unwrap();
        assert_eq!(annot(&conn, b).unwrap().2, 1, "starts as cross_dup");

        conn.execute(
            "DELETE FROM match_groups WHERE workspace_id = ?1",
            params![ws],
        )
        .unwrap();
        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(cross_dup_of(&conn, b), 0, "no longer cross_dup");
        assert_eq!(
            annot(&conn, b).map(|row| row.0).unwrap_or(0),
            0,
            "no longer a duplicate at all"
        );
    }

    #[test]
    fn file_duplicated_within_one_source_is_not_cross_dup() {
        let (mut conn, ws, src_a, _src_b) = setup();
        let a = insert_node(&conn, src_a, None, "copy1.jpg", "file", 100);
        let b = insert_node(&conn, src_a, None, "copy2.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);

        rebuild_annotations(&mut conn, ws).unwrap();

        let (has_dup, _, cross_dup, _, _) = annot(&conn, a).unwrap();
        assert_eq!(has_dup, 1);
        assert_eq!(cross_dup, 0);
    }

    #[test]
    fn directory_fully_cross_dup_is_flagged() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let a = insert_node(&conn, src_a, Some(dir), "photo.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "photo.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);

        rebuild_annotations(&mut conn, ws).unwrap();

        let (_, _, cross_dup, cross_dup_size, cross_dup_file_count) = annot(&conn, dir).unwrap();
        assert_eq!(cross_dup, 1);
        assert_eq!(cross_dup_size, 100, "whole subtree is cross-dup bytes");
        assert_eq!(cross_dup_file_count, 1);
    }

    #[test]
    fn directory_with_a_unique_file_is_not_cross_dup() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dup = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[dup, other]);
        insert_node(&conn, src_a, Some(dir), "unique.jpg", "file", 50);

        rebuild_annotations(&mut conn, ws).unwrap();

        // Not fully cross-dup (the unique file survives), but the byte/count
        // rollup still reports exactly the cross-dup portion so the UI can
        // show "1 file exclusive to this device" instead of the full total.
        let (_, _, cross_dup, cross_dup_size, cross_dup_file_count) = annot(&conn, dir).unwrap();
        assert_eq!(cross_dup, 0);
        assert_eq!(cross_dup_size, 100);
        assert_eq!(cross_dup_file_count, 1);
    }

    #[test]
    fn surviving_file_keeps_all_ancestor_directories_visible() {
        // root/
        //   cross_dup_file.jpg  (duplicated on device-b)
        //   nested/
        //     inner/
        //       unique.jpg      (no duplicate anywhere)
        let (mut conn, ws, src_a, src_b) = setup();
        let root = insert_node(&conn, src_a, None, "root", "directory", 0);
        let nested = insert_node(&conn, src_a, Some(root), "nested", "directory", 0);
        let inner = insert_node(&conn, src_a, Some(nested), "inner", "directory", 0);
        insert_node(&conn, src_a, Some(inner), "unique.jpg", "file", 50);

        let dup = insert_node(&conn, src_a, Some(root), "shared.jpg", "file", 100);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[dup, other]);

        rebuild_annotations(&mut conn, ws).unwrap();

        // `nested`/`inner` hold no duplicated content at all, so (like any
        // dup-free directory today) they get no dup_annot row -- get_tree's
        // COALESCE(d.cross_dup, 0) is what actually makes them "visible".
        assert_eq!(cross_dup_of(&conn, root), 0, "root");
        assert_eq!(cross_dup_of(&conn, nested), 0, "nested");
        assert_eq!(cross_dup_of(&conn, inner), 0, "inner");
    }

    #[test]
    fn symlink_keeps_directory_visible_and_is_never_annotated() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dup = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[dup, other]);
        let link = insert_node(&conn, src_a, Some(dir), "link.jpg", "link", 0);

        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(annot(&conn, dir).unwrap().2, 0, "symlink keeps dir visible");
        assert!(annot(&conn, link).is_none(), "symlinks are never annotated");
    }

    #[test]
    fn cross_dup_size_by_source_counts_only_cross_source_bytes() {
        let (conn, ws, src_a, src_b) = setup();
        // Cross-source pair: counts toward both sources.
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);
        // Internal-only duplicate on src_a: must NOT be counted here, unlike
        // `duplicated_size_by_source`'s pooled total.
        let c1 = insert_node(&conn, src_a, None, "copy1.png", "file", 30);
        let c2 = insert_node(&conn, src_a, None, "copy2.png", "file", 30);
        insert_file_group(&conn, ws, &[c1, c2]);

        let stats = cross_dup_size_by_source(&conn, ws).unwrap();
        assert_eq!(stats.get(&src_a), Some(&(100, 1)));
        assert_eq!(stats.get(&src_b), Some(&(100, 1)));
    }

    #[test]
    fn cross_dup_size_by_source_counts_a_file_once_even_in_multiple_groups() {
        // A file can legitimately persist in more than one match group across
        // tiers (e.g. a tier-D match with one partner and a separate tier-E
        // match with a different partner). Both groups are cross-source, but
        // `a`'s bytes must only be counted once for src_a, or the "exclusive
        // to this device" subtraction in the frontend goes negative.
        let (conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b1 = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        let b2 = insert_node(&conn, src_b, None, "shared_alt.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b1]);
        insert_file_group(&conn, ws, &[a, b2]);

        let stats = cross_dup_size_by_source(&conn, ws).unwrap();
        assert_eq!(stats.get(&src_a), Some(&(100, 1)), "a's bytes counted once");
        assert_eq!(stats.get(&src_b), Some(&(200, 2)));
    }

    #[test]
    fn duplicated_size_by_source_counts_a_file_once_even_in_multiple_groups() {
        // The `duplicated_size_by_source` counterpart to the
        // `cross_dup_size_by_source` test above. Without the per-node guard
        // `a`'s 100 bytes were billed to src_a once per group, so a source
        // could report more duplicated bytes than it physically holds and the
        // "% dup" badge printed over 100%.
        let (conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b1 = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        let b2 = insert_node(&conn, src_b, None, "shared_alt.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b1]);
        insert_file_group(&conn, ws, &[a, b2]);

        let dup = duplicated_size_by_source(&conn, ws).unwrap();
        assert_eq!(dup.get(&src_a), Some(&100), "a's bytes counted once");
        assert_eq!(dup.get(&src_b), Some(&200));
    }

    #[test]
    fn duplicated_size_by_source_still_counts_all_but_one_internal_copy() {
        // The per-node guard must not disturb the internal-duplicate rule:
        // three copies in one source means two are surplus. Each node is
        // distinct, so the guard never fires here.
        let (conn, ws, src_a, _src_b) = setup();
        let c1 = insert_node(&conn, src_a, None, "copy1.png", "file", 30);
        let c2 = insert_node(&conn, src_a, None, "copy2.png", "file", 30);
        let c3 = insert_node(&conn, src_a, None, "copy3.png", "file", 30);
        insert_file_group(&conn, ws, &[c1, c2, c3]);

        let dup = duplicated_size_by_source(&conn, ws).unwrap();
        assert_eq!(dup.get(&src_a), Some(&60), "3 copies -> 2 are surplus");
    }

    #[test]
    fn duplicated_size_by_source_is_measured_against_physical_not_logical_bytes() {
        // The regression that made a byte-for-byte duplicate device read as
        // ~63%. Every canonical file here is duplicated on the other source,
        // so the duplicated bytes must equal the source's `physical_size` --
        // not some fraction of a `total_size` inflated by hardlink aliases.
        let (conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);
        // An alias of `a`: same physical bytes under a second name. It is
        // excluded from the matcher, so it can never be in the numerator.
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, alias_of)
             VALUES (?1, NULL, 'shared_alias.jpg', 'shared_alias.jpg', 'file', 100, ?2)",
            params![src_a, a],
        )
        .unwrap();

        let dup = *duplicated_size_by_source(&conn, ws)
            .unwrap()
            .get(&src_a)
            .unwrap();
        // total_size would be 200 (both names), physical_size is 100.
        assert_eq!(dup, 100);
        assert_eq!(
            dup as f64 / 100.0 * 100.0,
            100.0,
            "against physical bytes this device is fully duplicated"
        );
        assert_eq!(
            dup as f64 / 200.0 * 100.0,
            50.0,
            "against logical bytes the same device would read as half -- the bug"
        );
    }

    /// Insert a hardlink alias of `canonical` -- a second name for the same
    /// bytes, which `load_file_locs` filters out and the matcher never sees.
    fn insert_alias(
        conn: &Connection,
        source_id: i64,
        parent_id: Option<i64>,
        name: &str,
        size: i64,
        canonical: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, alias_of)
             VALUES (?1, ?2, ?3, ?3, 'file', ?4, ?5)",
            params![source_id, parent_id, name, size, canonical],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn alias_inherits_its_canonical_cross_dup_annotation() {
        // Regression: aliases got no `dup_annot` row at all, so every consumer
        // reading `COALESCE(cross_dup, 0)` treated them as exclusive to this
        // device. A filtered consolidation drag then carried across a second
        // name for content whose canonical the same filter had just hidden.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let canonical = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        let alias = insert_alias(&conn, src_a, Some(dir), "shared_alias.jpg", 100, canonical);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[canonical, other]);

        build_folder_groups(&mut conn, ws).unwrap();
        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(
            cross_dup_of(&conn, canonical),
            1,
            "canonical is duplicated on another device"
        );
        assert_eq!(
            cross_dup_of(&conn, alias),
            1,
            "so its other name is equally non-exclusive and must hide with it"
        );
    }

    #[test]
    fn alias_bumps_cross_dup_count_but_not_cross_dup_size() {
        // The names/bytes split, at the directory rollup. Two names are hidden
        // by the funnel, but they are one file's worth of bytes -- and
        // `subtree_size` excludes alias bytes too, so counting them here would
        // make `subtree_size - cross_dup_size` subtract bytes never in the
        // total.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let canonical = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        insert_alias(&conn, src_a, Some(dir), "shared_alias.jpg", 100, canonical);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[canonical, other]);

        build_folder_groups(&mut conn, ws).unwrap();
        rebuild_annotations(&mut conn, ws).unwrap();

        let (_, _, _, cross_size, cross_count) = annot(&conn, dir).unwrap();
        assert_eq!(cross_count, 2, "both names are hidden, so both count");
        assert_eq!(cross_size, 100, "but they are one file's worth of bytes");
    }

    #[test]
    fn heavily_duplicated_directory_without_a_folder_group_is_not_in_folder_group() {
        // 90% of this directory's bytes are duplicated, but there is no
        // same-named sibling directory on another source for
        // `build_folder_groups` to cluster it with -- dup_pct alone must not
        // be mistaken for real folder-group membership.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dup = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 90);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 90);
        insert_file_group(&conn, ws, &[dup, other]);
        insert_node(&conn, src_a, Some(dir), "unique.jpg", "file", 10);

        // Mirrors the real pipeline order (build_folder_groups runs before
        // rebuild_annotations); with no same-named sibling on another
        // source, this is a no-op, which is exactly the point.
        build_folder_groups(&mut conn, ws).unwrap();
        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(in_folder_group_of(&conn, dir), 0);
    }

    #[test]
    fn clustered_folder_group_member_is_in_folder_group() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        let a = insert_node(&conn, src_a, Some(dir_a), "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, Some(dir_b), "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);
        // `build_folder_groups` already ran (and committed) earlier in the
        // real dedup pipeline before `rebuild_annotations` -- mirror that
        // order here rather than hand-inserting a folder match group, so
        // this test also exercises the real clustering logic.
        build_folder_groups(&mut conn, ws).unwrap();

        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(in_folder_group_of(&conn, dir_a), 1);
        assert_eq!(in_folder_group_of(&conn, dir_b), 1);
    }

    fn insert_file_with_mtime(
        conn: &Connection,
        source_id: i64,
        parent_id: Option<i64>,
        name: &str,
        size: i64,
        mtime: Option<&str>,
    ) -> i64 {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size, mtime, subtree_size, subtree_file_count)
             VALUES (?1, ?2, ?3, ?3, 'file', ?4, ?5, ?4, 1)",
            params![source_id, parent_id, name, size, mtime],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn listing_hash_of(conn: &Connection, node_id: i64) -> Vec<u8> {
        conn.query_row(
            "SELECT listing_hash FROM nodes WHERE id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn identical_subtrees_produce_equal_listing_hashes() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(listing_hash_of(&conn, dir_a), listing_hash_of(&conn, dir_b));

        let listing_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND primary_signal = 'listing'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(listing_groups, 1);
    }

    #[test]
    fn renamed_folder_still_matches_by_listing_hash() {
        // Same content, different directory name -- the case the byte-
        // overlap heuristic in `build_folder_groups` misses (it requires a
        // matching basename); the listing hash doesn't care what a folder
        // is named, only what it contains.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "vacation_photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "trip_2024", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(listing_hash_of(&conn, dir_a), listing_hash_of(&conn, dir_b));

        let listing_groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_groups WHERE workspace_id = ?1 AND primary_signal = 'listing'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            listing_groups, 1,
            "differently-named folders with identical content must still cluster"
        );
    }

    #[test]
    fn single_file_difference_changes_the_hash() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            101, // one byte larger
            Some("2024-01-01_10:00:00"),
        );

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_ne!(listing_hash_of(&conn, dir_a), listing_hash_of(&conn, dir_b));
    }

    #[test]
    fn two_second_mtime_noise_does_not_change_listing_hash() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:01"), // 1s -- within the same 2s bucket
        );

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(
            listing_hash_of(&conn, dir_a),
            listing_hash_of(&conn, dir_b),
            "FAT's 2s mtime granularity must not change the structural hash"
        );
    }

    #[test]
    fn whole_hour_offset_does_change_listing_hash() {
        // Unlike `dedup.rs::compare_mtime`'s tolerant comparison, a
        // structural hash must not absorb a whole-hour timezone shift --
        // that would risk two genuinely different-vintage folders colliding.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_11:00:00"),
        );

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_ne!(listing_hash_of(&conn, dir_a), listing_hash_of(&conn, dir_b));
    }

    #[test]
    fn listing_hash_cluster_requires_two_distinct_sources() {
        let (mut conn, ws, src_a, _src_b) = setup();
        let dir1 = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir2 = insert_node(&conn, src_a, None, "photos_copy", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir1),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir2),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );

        let count = compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(
            count, 0,
            "two identical directories within a single source must not form a listing group"
        );
    }

    #[test]
    fn empty_directories_never_cluster_with_each_other() {
        let (mut conn, ws, src_a, src_b) = setup();
        insert_node(&conn, src_a, None, "empty1", "directory", 0);
        insert_node(&conn, src_b, None, "empty2", "directory", 0);

        let count = compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(
            count, 0,
            "two unrelated empty directories must never be treated as a structural match"
        );
    }

    #[test]
    fn nested_identical_subtrees_emit_only_the_top_level_listing_group() {
        // src_a: photos/2024/a.jpg ; src_b: photos/2024/a.jpg -- both the
        // outer `photos` and the inner `2024` directories hash equal across
        // sources, but only the root of that match should get a group.
        let (mut conn, ws, src_a, src_b) = setup();
        let photos_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let year_a = insert_node(&conn, src_a, Some(photos_a), "2024", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(year_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );

        let photos_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        let year_b = insert_node(&conn, src_b, Some(photos_b), "2024", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(year_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );

        let count = compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(
            count, 1,
            "a 2-level identical tree must emit exactly one listing group, not one per level"
        );

        let member_ids: HashSet<i64> = conn
            .prepare(
                "SELECT mm.node_id FROM match_members mm
                 JOIN match_groups mg ON mg.id = mm.group_id
                 WHERE mg.workspace_id = ?1 AND mg.primary_signal = 'listing'",
            )
            .unwrap()
            .query_map(params![ws], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            member_ids,
            HashSet::from([photos_a, photos_b]),
            "the group must be on the top-level `photos` dirs, not the nested `2024` dirs"
        );
    }

    #[test]
    fn excluded_source_is_left_out_of_listing_hash_clustering() {
        // Two structurally identical directories, one on an excluded source:
        // the "needs >= 2 distinct sources" gate must not be satisfied by the
        // hidden source, so no listing group is emitted and the hidden dir
        // gets no listing_hash written.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        conn.execute(
            "UPDATE sources SET excluded = 1 WHERE id = ?1",
            params![src_a],
        )
        .unwrap();

        let count = compute_listing_hashes(&mut conn, ws).unwrap();
        assert_eq!(
            count, 0,
            "the excluded source must not count toward the 2-source gate"
        );

        let groups: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_members mm
                 JOIN match_groups mg ON mg.id = mm.group_id
                 JOIN nodes n ON n.id = mm.node_id
                 WHERE mg.workspace_id = ?1 AND n.source_id = ?2",
                params![ws, src_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            groups, 0,
            "no folder group may reference an excluded source's node"
        );

        let hash_a: Option<Vec<u8>> = conn
            .query_row(
                "SELECT listing_hash FROM nodes WHERE id = ?1",
                params![dir_a],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hash_a.is_none(), "excluded dir keeps its NULL listing_hash");
    }

    #[test]
    fn a_directory_with_a_symlink_does_not_match_one_without() {
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        insert_file_with_mtime(
            &conn,
            src_a,
            Some(dir_a),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        insert_file_with_mtime(
            &conn,
            src_b,
            Some(dir_b),
            "a.jpg",
            100,
            Some("2024-01-01_10:00:00"),
        );
        // Only dir_a has an extra symlink -- the two directories are
        // structurally different and must not cluster.
        insert_node(&conn, src_a, Some(dir_a), "link.jpg", "link", 0);

        compute_listing_hashes(&mut conn, ws).unwrap();
        assert_ne!(
            listing_hash_of(&conn, dir_a),
            listing_hash_of(&conn, dir_b),
            "a directory with a symlink must not hash identically to one without"
        );
    }
}
