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
struct FileLoc {
    node_id: i64,
    source_id: i64,
    parent_id: Option<i64>,
    size: i64,
}

/// Build folder-level match groups. Returns the number of folder groups written.
pub fn build_folder_groups(conn: &mut Connection, workspace_id: i64) -> rusqlite::Result<usize> {
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

    // Load all file nodes with their parent + size.
    let files = load_file_locs(conn, workspace_id)?;
    // Load directory parent chain so we can walk a file's ancestor folders.
    let parent_of = load_parent_of(conn, workspace_id)?;
    let source_of = load_source_of(conn, workspace_id)?;

    // For each directory: total subtree file bytes, and duplicated bytes (files
    // whose match group also contains a file from a different source), plus the
    // set of "partner" directories those matched files live in.
    let mut dir_total: HashMap<i64, i64> = HashMap::new();
    let mut dir_dup: HashMap<i64, i64> = HashMap::new();

    // group_id -> list of (source_id, parent_dir_id) to detect cross-source dup.
    let mut group_sources: HashMap<i64, HashSet<i64>> = HashMap::new();
    for f in &files {
        if let Some(gid) = file_group.get(&f.node_id) {
            group_sources.entry(*gid).or_default().insert(f.source_id);
        }
    }

    for f in &files {
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

fn load_file_locs(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<FileLoc>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.source_id, n.parent_id, n.size FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'file'",
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
         WHERE s.workspace_id = ?1 AND n.type IN ('file', 'link')",
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

fn load_parent_of(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, Option<i64>>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'directory'",
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
         WHERE s.workspace_id = ?1 AND n.type = 'directory'",
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
         WHERE s.workspace_id = ?1 AND n.type = 'directory'",
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
pub fn duplicated_size_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, i64>> {
    // group_id -> set of source ids and list of (group, source, size).
    let mut group_srcs: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut members: Vec<(i64, i64, i64)> = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT mm.group_id, n.source_id, n.size FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (gid, src, size) = row?;
        group_srcs.entry(gid).or_default().insert(src);
        members.push((gid, src, size));
    }

    // For internal-only groups we count all-but-one copy per source, so we
    // track (group, source) -> member count seen so far.
    let mut seen: HashMap<(i64, i64), i64> = HashMap::new();
    let mut result: HashMap<i64, i64> = HashMap::new();
    for (gid, src, size) in members {
        let cross = group_srcs.get(&gid).map(|s| s.len() > 1).unwrap_or(false);
        if cross {
            *result.entry(src).or_insert(0) += size;
        } else {
            // Internal duplicates: first copy is the "original", the rest count.
            let count = seen.entry((gid, src)).or_insert(0);
            if *count > 0 {
                *result.entry(src).or_insert(0) += size;
            }
            *count += 1;
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
    let mut members: Vec<(i64, i64, i64)> = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT mm.group_id, n.source_id, n.size FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file'",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (gid, src, size) = row?;
        group_srcs.entry(gid).or_default().insert(src);
        members.push((gid, src, size));
    }

    let mut result: HashMap<i64, (i64, i64)> = HashMap::new();
    for (gid, src, size) in members {
        let cross = group_srcs.get(&gid).map(|s| s.len() > 1).unwrap_or(false);
        if cross {
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
pub fn rebuild_annotations(conn: &mut Connection, workspace_id: i64) -> rusqlite::Result<()> {
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

    let files = load_file_locs(conn, workspace_id)?;
    let parent_of = load_parent_of(conn, workspace_id)?;

    // Roll duplicated bytes up the ancestor chain.
    let mut dir_dup: HashMap<i64, i64> = HashMap::new();
    for f in &files {
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
    for f in &files {
        if let Some(gid) = file_group.get(&f.node_id) {
            group_sources.entry(*gid).or_default().insert(f.source_id);
        }
    }
    let cross_dup_files: HashSet<i64> = files
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

    // Directory-level cross_dup is a leaf-*count* rollup (not byte-weighted
    // like dup_pct above): a directory is only fully cross-dup when every
    // leaf (file or symlink) in its subtree is a cross_dup file. Symlinks
    // are never in cross_dup_files (dedup matching only loads type='file'
    // rows), so a directory holding even one symlink can never read as
    // fully cross-dup, keeping it visible under the filter.
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
    for f in &files {
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
             WHERE s.workspace_id = ?1 AND n.type = 'directory'",
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
}
