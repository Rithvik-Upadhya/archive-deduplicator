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

/// Rebuild the `dup_annot` cache: per-file duplicate flags and per-directory
/// duplicated-percentage rollups. Called once after each dedup run so that
/// `get_tree` can serve annotations with a simple JOIN instead of recomputing
/// the whole workspace on every expand.
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
        let mut stmt =
            tx.prepare("INSERT INTO dup_annot (node_id, has_dup, dup_pct) VALUES (?1, ?2, ?3)")?;
        for id in &dup_files {
            stmt.execute(params![id, 1, 0.0])?;
        }
        for (dir, dup) in &dir_dup {
            let total = *dir_sizes.get(dir).unwrap_or(&0);
            let pct = if total > 0 {
                *dup as f64 / total as f64 * 100.0
            } else {
                0.0
            };
            stmt.execute(params![dir, (pct > 0.0) as i64, pct])?;
        }
    }
    tx.commit()?;
    Ok(())
}
