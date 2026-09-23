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
    let cross_source = load_cross_source_files(conn, workspace_id)?;
    build_folder_groups_with(conn, workspace_id, &files, &parent_of, &cross_source)
}

/// A directory that passed the folder rollup's 80% test.
struct DupDir {
    id: i64,
    /// Subtree file bytes.
    total: i64,
    /// Of those, bytes in cross-source file groups.
    dup: i64,
    /// `dup`, each file's bytes weighted by its best cross-source confidence.
    weighted: f64,
}

pub(crate) fn build_folder_groups_with(
    conn: &mut Connection,
    workspace_id: i64,
    files: &[FileLoc],
    parent_of: &HashMap<i64, Option<i64>>,
    cross_source: &CrossSource,
) -> rusqlite::Result<usize> {
    let source_of = load_source_of(conn, workspace_id)?;

    // For each directory: total subtree file bytes, duplicated bytes (files
    // whose match group also contains a file from a different source), and
    // those duplicated bytes weighted by each file's best cross-source
    // confidence -- the numerator of the group's confidence below.
    let mut dir_total: HashMap<i64, i64> = HashMap::new();
    let mut dir_dup: HashMap<i64, i64> = HashMap::new();
    let mut dir_dup_weighted: HashMap<i64, f64> = HashMap::new();

    for f in files {
        // Accumulate this file's size into every ancestor directory's total.
        let mut cur = f.parent_id;
        while let Some(dir) = cur {
            *dir_total.entry(dir).or_insert(0) += f.size;
            cur = parent_of.get(&dir).copied().flatten();
        }
        // Is this file duplicated across sources? See
        // `load_cross_source_files` for why this must consider *every* group
        // the file belongs to, not one of them.
        if let Some(conf) = cross_source.confidence(f.node_id) {
            let mut cur = f.parent_id;
            while let Some(dir) = cur {
                *dir_dup.entry(dir).or_insert(0) += f.size;
                *dir_dup_weighted.entry(dir).or_insert(0.0) += f.size as f64 * conf;
                cur = parent_of.get(&dir).copied().flatten();
            }
        }
    }

    // A directory qualifies as a "duplicated folder" when >= 80% of its subtree
    // bytes are cross-source duplicated and it holds a meaningful amount of data.
    // That 80% decides *whether* a folder is reported; how *sure* the report is
    // comes from the evidence beneath it (see the confidence below).
    let mut dup_dirs: Vec<DupDir> = Vec::new();
    for (dir, total) in &dir_total {
        if *total <= 0 {
            continue;
        }
        let dup = *dir_dup.get(dir).unwrap_or(&0);
        if dup as f64 / *total as f64 >= 0.8 {
            dup_dirs.push(DupDir {
                id: *dir,
                total: *total,
                dup,
                weighted: *dir_dup_weighted.get(dir).unwrap_or(&0.0),
            });
        }
    }

    // Cluster duplicated directories that share the same basename across
    // different sources into folder groups.
    let dir_names = load_dir_names(conn, workspace_id)?;
    let mut by_name: HashMap<String, Vec<DupDir>> = HashMap::new();
    for d in dup_dirs {
        let dir = d.id;
        // Skip a directory if its own parent is also a fully-duplicated dir, to
        // report duplication at the highest folder level rather than every level.
        if let Some(Some(parent)) = parent_of.get(&dir).copied()
            && dir_total.contains_key(&parent)
        {
            let ptotal = dir_total[&parent];
            let pdup = *dir_dup.get(&parent).unwrap_or(&0);
            if ptotal > 0 && (pdup as f64 / ptotal as f64) >= 0.8 {
                continue;
            }
        }
        if let Some(name) = dir_names.get(&dir) {
            by_name.entry(name.to_lowercase()).or_default().push(d);
        }
    }

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for (_name, mut dirs) in by_name {
        // Need the same-named folder present in at least two different sources.
        let distinct_sources: HashSet<i64> = dirs
            .iter()
            .filter_map(|d| source_of.get(&d.id).copied())
            .collect();
        if dirs.len() < 2 || distinct_sources.len() < 2 {
            continue;
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.total));
        // Confidence is the byte-weighted mean of the cross-source file
        // matches beneath the member folders -- mostly hash-backed reads ~100,
        // mostly tier-E reads ~45. It used to be the average *coverage*
        // (80-100), which scored a folder backed only by 45% matches the same
        // as a full-content hash and made the min-confidence cutoff meaningless
        // for folders. Coverage is still shown, as `dup_annot.dup_pct`. Every
        // member passed the 80% test with a positive total, so `dup > 0`.
        let dup_bytes: i64 = dirs.iter().map(|d| d.dup).sum();
        let weighted: f64 = dirs.iter().map(|d| d.weighted).sum();
        let confidence = weighted / dup_bytes as f64;
        let max_total = dirs.iter().map(|d| d.total).max().unwrap_or(0);

        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'folder', ?2, 'folder', ?3)",
            params![workspace_id, confidence, max_total],
        )?;
        let gid = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            )?;
            for d in &dirs {
                stmt.execute(params![gid, d.id])?;
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

/// Confidence assigned to a listing-hash folder match: equal to tier C in
/// `dedup.rs`'s metadata scale, because a match means *every* file in the
/// subtree agrees on name, size and 2s-rounded mtime -- tier-C evidence for
/// each one -- and the structure agrees too. Never "Confirmed": no byte
/// content is verified. It was 60, below a single tier-C file, which
/// undersold every real match; the trivial matches that 60 was hedging
/// against are now kept out by the content guard in `compute_listing_hashes`.
const LISTING_HASH_CONFIDENCE: f64 = 70.0;

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
///
/// A folder only seeds a group when its subtree holds at least two names and
/// at least `min_size_bytes` of file data. Structure alone is no evidence for
/// a folder holding one tiny file: every `desktop.ini`-only folder in the
/// workspace would otherwise match every other at tier-C confidence.
/// `min_size_bytes` is the user's matching threshold, so a folder too small
/// to hold one matchable file is not matched as a whole either.
pub fn compute_listing_hashes(
    conn: &mut Connection,
    workspace_id: i64,
    min_size_bytes: i64,
) -> rusqlite::Result<usize> {
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
    // Subtree totals over exactly what the hash reads (canonical files and
    // symlinks), for the content guard and the group's size.
    let mut sub_names: Vec<i64> = vec![0; dirs.len()];
    let mut sub_bytes: Vec<i64> = vec![0; dirs.len()];
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
                sub_names[i] += 1;
                sub_bytes[i] += size;
            }
        }
        if let Some(links) = links_by_parent.get(&dir_id) {
            for (name, link_target) in links {
                let mut bytes = Vec::with_capacity(name.len() + 16);
                bytes.extend_from_slice(b"link:");
                bytes.extend_from_slice(name.as_bytes());
                bytes.extend_from_slice(link_target.as_deref().unwrap_or_default().as_bytes());
                entries.push((name.clone(), bytes));
                sub_names[i] += 1;
            }
        }
        if let Some(child_idxs) = children_dirs.get(&dir_id) {
            for &ci in child_idxs {
                let mut bytes = Vec::with_capacity(dirs[ci].name.len() + 37);
                bytes.extend_from_slice(b"dir:");
                bytes.extend_from_slice(dirs[ci].name.as_bytes());
                bytes.extend_from_slice(&listing_hash[ci]);
                entries.push((dirs[ci].name.clone(), bytes));
                // Children sort deeper, so their totals are already final.
                sub_names[i] += sub_names[ci];
                sub_bytes[i] += sub_bytes[ci];
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
        // Too little content for structure to mean anything (see the doc
        // comment). A parent's totals are >= its child's, so a folder that
        // passes this implies its parent does too, and the root-only
        // emission below is unaffected.
        if sub_names[i] < 2 || sub_bytes[i] < min_size_bytes {
            continue;
        }
        by_hash.entry(listing_hash[i]).or_default().push(d.id);
    }

    let dir_parent: HashMap<i64, Option<i64>> = dirs.iter().map(|d| (d.id, d.parent_id)).collect();
    let dir_bytes: HashMap<i64, i64> = dirs
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id, sub_bytes[i]))
        .collect();

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
        // Sized like the byte-overlap groups (largest member's subtree bytes)
        // so the group list's min-size filter treats both kinds alike. It was
        // 0, which hid every listing group under the default 64 KB filter.
        let size = root_ids
            .iter()
            .filter_map(|d| dir_bytes.get(d).copied())
            .max()
            .unwrap_or(0);
        tx.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'folder', ?2, 'listing', ?3)",
            params![workspace_id, LISTING_HASH_CONFIDENCE, size],
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

/// Files sitting in at least one *cross-source* `kind='file'` match group --
/// the set backing both the folder rollup's 80%-overlap heuristic and
/// `dup_annot.cross_dup` (the "exclusive to this device" funnel) -- each with
/// the highest confidence among those cross-source groups: how sure we are
/// that the file exists elsewhere, which the folder rollup weights its
/// confidence by. Membership and confidence come from the same pass, so the
/// two can never disagree about which files count.
///
/// Built from the raw `match_members` rows, and it must stay that way. A file
/// can legitimately belong to several groups across tiers (a tier-C match with
/// one partner, a tier-E match with another), and it is cross-source when
/// **any** of them spans more than one source. Both call sites used to keep a
/// `HashMap<node_id, group_id>` filled with `insert`, collapsing a file to one
/// arbitrary group, and then derive each group's source set from that collapsed
/// map -- so a group only counted the members that happened to select it, and a
/// genuine cross-source pair could read as single-source from both ends. The
/// errors only ever lose cross-ness, never invent it, which is why the symptom
/// was silent: 2,562 of 30,696 canonical files marked exclusive-to-this-device
/// while sitting in a cross-source group, with no false positives to notice.
///
/// The confidence is a maximum over **every** cross-source group the file is
/// in, for the same reason: taking it from one arbitrary group would score a
/// file hash-matched on another device by its weakest pairing instead.
///
/// `s.excluded = 0` mirrors `load_file_locs`: a hidden source must not be able
/// to make a group look cross-source.
pub(crate) fn load_cross_source_files(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<CrossSource> {
    let mut stmt = conn.prepare(
        "SELECT mm.node_id, mm.group_id, n.source_id, mg.confidence FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         JOIN sources s ON s.id = n.source_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })?;
    let mut group_sources: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut node_groups: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    for row in rows {
        let (node_id, group_id, source_id, confidence) = row?;
        group_sources.entry(group_id).or_default().insert(source_id);
        node_groups
            .entry(node_id)
            .or_default()
            .push((group_id, confidence));
    }
    let mut cross: HashMap<i64, f64> = node_groups
        .into_iter()
        .filter_map(|(node_id, groups)| {
            groups
                .iter()
                .filter(|(g, _)| group_sources.get(g).map(|s| s.len() > 1).unwrap_or(false))
                .map(|&(_, confidence)| confidence)
                .reduce(f64::max)
                .map(|best| (node_id, best))
        })
        .collect();

    // Hardlink aliases inherit their canonical's answer, *here*, so that every
    // consumer shares one definition of "duplicated on another source". An
    // alias is never in `match_members` -- it never reaches the matcher -- so
    // without this each caller has to remember to extend the set itself, and
    // the device header and the tree drift apart: the header once reported
    // 5,071 exclusive files where the tree showed 1,322, the gap being exactly
    // the 3,749 aliases one path knew about and the other did not.
    let aliases: Vec<(i64, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.alias_of FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.alias_of IS NOT NULL AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (alias_id, canonical_id) in &aliases {
        if let Some(&confidence) = cross.get(canonical_id) {
            cross.insert(*alias_id, confidence);
        }
    }
    Ok(CrossSource(cross))
}

/// The answer of `load_cross_source_files`: which nodes have a duplicate on
/// another source, and the best confidence for that.
pub(crate) struct CrossSource(HashMap<i64, f64>);

impl CrossSource {
    /// Whether the node is duplicated on another source.
    pub(crate) fn contains(&self, node_id: &i64) -> bool {
        self.0.contains_key(node_id)
    }

    /// The highest confidence among the node's cross-source groups, or
    /// `None` when it has none.
    pub(crate) fn confidence(&self, node_id: i64) -> Option<f64> {
        self.0.get(&node_id).copied()
    }
}

pub(crate) fn load_file_locs(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<Vec<FileLoc>> {
    // `s.excluded = 0` mirrors `dedup.rs::load_files`: an excluded source is out
    // of the matcher entirely, so its files must not feed folder rollups or the
    // `dup_annot` cache either.
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id, n.size FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type = 'file' AND n.alias_of IS NULL AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(FileLoc {
            node_id: r.get(0)?,
            parent_id: r.get(1)?,
            size: r.get(2)?,
        })
    })?;
    rows.collect()
}

/// A file or symlink used for directory-level leaf-count rollups. Distinct
/// from `FileLoc`, which is byte-weighted and file-only -- this counts every
/// leaf (files *and* symlinks), so a directory counts as fully duplicated only
/// when its symlinks are accounted for too, not merely all its *files*. Since
/// symlinks are matchable (`dedup.rs`'s `Tier::S`), that is now a real
/// question rather than a guaranteed "no".
struct LeafLoc {
    node_id: i64,
    parent_id: Option<i64>,
}

fn load_leaf_locs(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<LeafLoc>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.parent_id FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type IN ('file', 'link') AND n.alias_of IS NULL AND s.excluded = 0",
    )?;
    // `n.type` is deliberately not carried onto `LeafLoc`: every row the
    // `WHERE` admits counts the same, files and symlinks alike. Selecting it
    // would invite a caller to reintroduce the file-only restriction this type
    // exists to drop.
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok(LeafLoc {
            node_id: r.get(0)?,
            parent_id: r.get(1)?,
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
///
/// Excluded sources are left out entirely (`s.excluded = 0`, matching
/// `load_cross_source_files` and `cross_dup_size_by_source`). That has two
/// consequences, and both are the point: an excluded device reports no
/// duplication of its own, and a group whose only other member lives on an
/// excluded device is no longer "cross-source" -- so the remaining copy is the
/// original, not surplus. Without this the badge counted redundancy against a
/// device the user had switched off, and disagreed with the funnel beside it
/// about whether a file had a partner at all.
/// Duplicated bytes for one source, split by where the other copies live.
///
/// `internal + cross` is exactly the pooled total this function has always
/// returned, because both buckets are filled from one pass under one
/// `counted` guard -- so the two segments of the source bar always sum to the
/// "% dup" badge beside them.
///
/// `cross` here is still **not** `CrossDupStats::size` (what the "exclusive to
/// this device" funnel hides). Both now agree on excluded sources, but that one
/// additionally folds in hardlink aliases and ranges over `nodes` rather than
/// `match_members`. Take the split from here; don't reconstruct it by
/// subtracting the funnel's figure.
#[derive(Debug, Default, Clone, Copy)]
pub struct DupSplit {
    pub internal: i64,
    pub cross: i64,
}

impl DupSplit {
    pub fn total(&self) -> i64 {
        self.internal + self.cross
    }
}

pub fn duplicated_size_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, DupSplit>> {
    // group_id -> set of source ids and list of (group, node, source, bytes).
    // `bytes` is what this node may contribute, already zeroed for anything
    // that holds no content of its own -- see the guard in the query below.
    let mut group_srcs: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut members: Vec<(i64, i64, i64, i64)> = Vec::new();

    let mut stmt = conn.prepare(
        "SELECT mm.group_id, mm.node_id, n.source_id, n.size, n.type FROM match_members mm
         JOIN match_groups mg ON mg.id = mm.group_id
         JOIN nodes n ON n.id = mm.node_id
         JOIN sources s ON s.id = n.source_id
         WHERE mg.workspace_id = ?1 AND mg.kind = 'file' AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        let size: i64 = r.get(3)?;
        let node_type: String = r.get(4)?;
        // Counts count names, sizes count bytes held. Symlinks are matchable
        // (dedup.rs's tier S) and so reach `match_members`, but they hold no
        // content of their own -- `nodes.size` for one is the length of its
        // target path, and `total_size`/`physical_size` never took those bytes
        // in. Billing them here would put bytes in the numerator that the base
        // this ratio divides by cannot hold. The row is kept rather than
        // filtered in SQL so a group's source set stays complete: today a
        // symlink group is pure symlinks, but a filter would silently change
        // cross-source determination if that ever stopped being true.
        //
        // Mirrors the same guard in `cross_dup_size_by_source` below.
        let bytes = if node_type == "file" { size } else { 0 };
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            bytes,
        ))
    })?;
    for row in rows {
        let (gid, node_id, src, bytes) = row?;
        group_srcs.entry(gid).or_default().insert(src);
        members.push((gid, node_id, src, bytes));
    }

    // For internal-only groups we count all-but-one copy per source, so we
    // track (group, source) -> member count seen so far.
    let mut seen: HashMap<(i64, i64), i64> = HashMap::new();
    // As in `cross_dup_size_by_source` below: a file may sit in more than one
    // match group across tiers (see dedup.rs's note on mixed groups), so count
    // each node's bytes at most once per source. Without this the total can
    // exceed the source's own physical size and the badge prints over 100%.
    let mut counted: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut result: HashMap<i64, DupSplit> = HashMap::new();
    for (gid, node_id, src, bytes) in members {
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
            // One `counted` guard across both buckets: a node billed under one
            // group is not billed again under another, and whichever group
            // bills it decides which half it lands in. That keeps
            // `internal + cross` equal to the old pooled total.
            let entry = result.entry(src).or_default();
            if cross {
                entry.cross += bytes;
            } else {
                entry.internal += bytes;
            }
        }
    }
    Ok(result)
}

/// What the "exclusive to this device" funnel hides, per source.
#[derive(Debug, Default, Clone, Copy)]
pub struct CrossDupStats {
    /// Canonical-file bytes hidden. Subtract from `physical_size`.
    pub size: i64,
    /// *Names* hidden -- canonical files, hardlink aliases and symlinks alike.
    /// Subtract from `file_count`.
    pub file_count: i64,
    /// Hidden bytes that belong to hardlink aliases. Not part of `size` (they
    /// were never in `physical_size` either); this exists so the header's
    /// "N hardlinked" annotation can shrink to match what is still on screen
    /// instead of always describing the whole device.
    pub alias_bytes: i64,
}

/// Cross-device duplicate bytes and file count per source. Unlike
/// `duplicated_size_by_source` (which pools cross-source and internal-only
/// duplication together for the "% dup" badges), this counts only bytes/files
/// whose match group spans more than one source -- exactly what the
/// "exclusive to this device" filter hides at the device-header level.
pub fn cross_dup_size_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, CrossDupStats>> {
    // Built from `load_cross_source_files` rather than from a second walk of
    // `match_members`, so the device header cannot disagree with the tree's
    // `dup_annot.cross_dup` about which nodes the funnel hides. Scanning the
    // rows again here is what let the two drift apart before.
    let cross = load_cross_source_files(conn, workspace_id)?;

    let mut stmt = conn.prepare(
        "SELECT n.id, n.source_id, n.size, n.type, n.alias_of IS NOT NULL FROM nodes n
         JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND n.type IN ('file', 'link') AND s.excluded = 0",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })?;
    let mut result: HashMap<i64, CrossDupStats> = HashMap::new();
    for row in rows {
        let (node_id, source_id, size, node_type, is_alias) = row?;
        if !cross.contains(&node_id) {
            continue;
        }
        let e = result.entry(source_id).or_default();
        // Counts count names, sizes count bytes held (see CLAUDE.md). Every
        // hidden name decrements what the header shows; only a canonical file
        // took bytes out of `physical_size`, so only it can put them back.
        e.file_count += 1;
        if is_alias {
            e.alias_bytes += size;
        } else if node_type == "file" {
            e.size += size;
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
    let cross_source = load_cross_source_files(conn, workspace_id)?;
    rebuild_annotations_with(
        conn,
        workspace_id,
        &files,
        &parent_of,
        &cross_source,
        &HashSet::new(),
    )
}

pub(crate) fn rebuild_annotations_with(
    conn: &mut Connection,
    workspace_id: i64,
    files: &[FileLoc],
    parent_of: &HashMap<i64, Option<i64>>,
    cross_source: &CrossSource,
    skipped: &HashSet<i64>,
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

    // Symlinks are matched now (`dedup.rs` groups them on `(name, target)` at
    // `Tier::S`), so they reach the rollups below. They behave like aliases in
    // the one way that matters here: each is a *name* the user has to deal
    // with, but holds no content bytes of its own -- `subtree_size` excludes
    // them, so anything subtracted from `subtree_size` must exclude them too.
    let symlinks: Vec<(i64, Option<i64>)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.parent_id FROM nodes n
             JOIN sources s ON s.id = n.source_id
             WHERE s.workspace_id = ?1 AND n.type = 'link' AND s.excluded = 0",
        )?;
        let rows = stmt.query_map(params![workspace_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

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

    // A file is a *cross-device* duplicate when any of its match groups holds a
    // member from a different source_id -- see `load_cross_source_files`, which
    // is restricted to file-kind groups so folder groups' directory members
    // don't leak into this check, and which already folds in the aliases.
    let cross_dup_files = cross_source;

    // Directory-level cross_dup is a leaf-*count* rollup (not byte-weighted
    // like dup_pct above): a directory is only fully cross-dup when every name
    // in its subtree -- file, symlink or hardlink alias alike -- is hidden.
    //
    // All three can be hidden now, which was not always true. Symlinks used to
    // be absent from `cross_dup_files` entirely (the matcher loaded only
    // `type='file'` rows), so a directory holding even one stayed visible under
    // the filter no matter what -- presented as exclusive-to-this-device on the
    // strength of a node the matcher had never examined. Aliases inherit their
    // canonical's flag for the same reason. A directory whose every name is
    // accounted for now correctly disappears whole.
    let leaves = load_leaf_locs(conn, workspace_id)?;
    let mut dir_leaf_total: HashMap<i64, i64> = HashMap::new();
    let mut dir_leaf_cross: HashMap<i64, i64> = HashMap::new();
    for leaf in &leaves {
        let mut cur = leaf.parent_id;
        while let Some(dir) = cur {
            *dir_leaf_total.entry(dir).or_insert(0) += 1;
            // No `leaf.is_file` restriction: a symlink duplicated on another
            // source is just as hidden by the funnel as a file is.
            if cross_dup_files.contains(&leaf.node_id) {
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
    // No `total > 0` guard: a directory with no leaves at all is vacuously
    // "every name under it is hidden", and that is the answer the funnel
    // wants. An empty directory holds nothing exclusive to this device, so
    // showing it -- as `.git/branches` and 152 others used to be, reporting
    // "0 files" -- is pure noise in a view whose whole job is to narrow down
    // to what is unique.
    let dir_cross_dup = |dir: &i64| -> bool {
        let total = *dir_leaf_total.get(dir).unwrap_or(&0);
        dir_leaf_cross.get(dir).copied().unwrap_or(0) == total
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
    // Symlinks, for the same reason and with the same asymmetry: a hidden
    // symlink is one fewer name to deal with, and zero fewer bytes.
    for (node_id, parent_id) in &symlinks {
        if !cross_dup_files.contains(node_id) {
            continue;
        }
        let mut cur = *parent_id;
        while let Some(dir) = cur {
            *dir_cross_count.entry(dir).or_insert(0) += 1;
            cur = parent_of.get(&dir).copied().flatten();
        }
    }

    // Nodes a safety cap declined to judge, rolled up so a directory can say
    // how many it holds without the user expanding to the files themselves --
    // 1,271 of them under one `target/debug/build` in the reference workspace.
    let mut dir_skipped: HashMap<i64, i64> = HashMap::new();
    for f in files {
        if !skipped.contains(&f.node_id) {
            continue;
        }
        let mut cur = f.parent_id;
        while let Some(dir) = cur {
            *dir_skipped.entry(dir).or_insert(0) += 1;
            cur = parent_of.get(&dir).copied().flatten();
        }
    }
    for (node_id, parent_id) in &symlinks {
        if !skipped.contains(node_id) {
            continue;
        }
        let mut cur = *parent_id;
        while let Some(dir) = cur {
            *dir_skipped.entry(dir).or_insert(0) += 1;
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
            "INSERT INTO dup_annot (node_id, has_dup, dup_pct, cross_dup, cross_dup_size, cross_dup_file_count, in_folder_group, skipped, skipped_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        // A skipped node is by definition in no group, so it is absent from
        // `dup_files` and would otherwise get no row at all -- and then
        // `get_tree`'s COALESCE would report it as an ordinary unique file,
        // which is exactly the claim the marker exists to stop.
        let annotated_files: HashSet<i64> = dup_files.union(skipped).copied().collect();
        for id in &annotated_files {
            stmt.execute(params![
                id,
                dup_files.contains(id) as i64,
                0.0,
                cross_dup_files.contains(id) as i64,
                0,
                0,
                0,
                skipped.contains(id) as i64,
                0
            ])?;
        }
        // Every directory, not just those in `dir_dup`. A directory only
        // entered `dir_dup` by containing duplicated bytes, so an empty one
        // got no row at all and `get_tree`'s `COALESCE(cross_dup, 0)` read it
        // as exclusive-to-this-device forever. `dir_sizes` is already the full
        // set of directories in the workspace.
        for dir in dir_sizes.keys() {
            let dup = dir_dup.get(dir).copied().unwrap_or(0);
            let total = *dir_sizes.get(dir).unwrap_or(&0);
            let pct = if total > 0 {
                dup as f64 / total as f64 * 100.0
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
                0,
                dir_skipped.get(dir).copied().unwrap_or(0),
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
    fn unmatched_symlink_keeps_its_directory_visible() {
        // Every *file* here is duplicated elsewhere, but the symlink is not
        // matched, so the directory still holds something exclusive and must
        // stay visible under the funnel. This used to hold trivially --
        // symlinks could never be matched at all -- and now holds for the
        // right reason: this particular symlink has no partner.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dup = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[dup, other]);
        let link = insert_node(&conn, src_a, Some(dir), "link.jpg", "link", 0);

        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(annot(&conn, dir).unwrap().2, 0, "symlink keeps dir visible");
        assert!(
            annot(&conn, link).is_none(),
            "an unmatched symlink gets no annotation"
        );
    }

    #[test]
    fn cross_dup_symlink_bumps_the_count_but_not_the_size() {
        // A matched symlink is a name the funnel hides, so it must decrement
        // the visible count -- but it holds no content bytes of its own and is
        // absent from `subtree_size`, so adding its `size` (the target-path
        // length) to `cross_dup_size` would subtract bytes that were never in
        // the total. Same asymmetry as a hardlink alias.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dup = insert_node(&conn, src_a, Some(dir), "shared.jpg", "file", 100);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[dup, other]);
        let link = insert_node(&conn, src_a, Some(dir), "link.jpg", "link", 40);
        let link_b = insert_node(&conn, src_b, None, "link.jpg", "link", 40);
        insert_file_group(&conn, ws, &[link, link_b]);

        rebuild_annotations(&mut conn, ws).unwrap();

        let (_, _, cross_dup, cross_size, cross_count) = annot(&conn, dir).unwrap();
        assert_eq!(cross_count, 2, "the file and the symlink are both hidden");
        assert_eq!(cross_size, 100, "only the file contributed bytes");
        assert_eq!(
            cross_dup, 1,
            "every leaf is accounted for, so the directory disappears whole"
        );
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
        assert_eq!((stats[&src_a].size, stats[&src_a].file_count), (100, 1));
        assert_eq!((stats[&src_b].size, stats[&src_b].file_count), (100, 1));
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
        assert_eq!(
            (stats[&src_a].size, stats[&src_a].file_count),
            (100, 1),
            "a's bytes counted once"
        );
        assert_eq!((stats[&src_b].size, stats[&src_b].file_count), (200, 2));
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
        assert_eq!(
            dup.get(&src_a).copied().unwrap_or_default().total(),
            100,
            "a's bytes counted once"
        );
        assert_eq!(dup.get(&src_b).copied().unwrap_or_default().total(), 200);
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
        assert_eq!(
            dup.get(&src_a).copied().unwrap_or_default().total(),
            60,
            "3 copies -> 2 are surplus"
        );
    }

    #[test]
    fn duplicated_size_by_source_splits_internal_from_cross_source() {
        // The two halves the source bar draws. Source A carries both kinds at
        // once: an internal-only pair (one surplus copy) and a file that also
        // lives on source B. They must land in different buckets, and their
        // sum must still be the pooled total the badge beside the bar shows --
        // otherwise the bar's two segments would not add up to it.
        let (conn, ws, src_a, src_b) = setup();

        // Internal-only: two copies on A, so 30 bytes are surplus.
        let i1 = insert_node(&conn, src_a, None, "inner1.png", "file", 30);
        let i2 = insert_node(&conn, src_a, None, "inner2.png", "file", 30);
        insert_file_group(&conn, ws, &[i1, i2]);

        // Cross-source: every copy is redundant somewhere else, so A's full
        // 100 bytes count.
        let x_a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let x_b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[x_a, x_b]);

        let dup = duplicated_size_by_source(&conn, ws).unwrap();
        let a = dup.get(&src_a).copied().unwrap_or_default();
        assert_eq!(a.internal, 30, "one surplus copy of the internal pair");
        assert_eq!(a.cross, 100, "the cross-source file's bytes");
        assert_eq!(a.total(), 130, "the pooled total the badge shows");

        // B has only the cross-source half.
        let b = dup.get(&src_b).copied().unwrap_or_default();
        assert_eq!(b.internal, 0);
        assert_eq!(b.cross, 100);
    }

    #[test]
    fn duplicated_size_by_source_bills_no_bytes_for_symlinks() {
        // Symlinks became matchable (tier S), so they reach `match_members` and
        // this function billed their `nodes.size` -- which on a live scan is
        // the length of the target path, not content. `total_size` and
        // `physical_size` are files-only, so those bytes went into the
        // numerator of a ratio whose base could never hold them.
        //
        // The real file pair is here so the assertion cannot pass merely
        // because nothing was counted at all.
        let (conn, ws, src_a, _src_b) = setup();

        let l1 = insert_node(&conn, src_a, None, "link1", "link", 42);
        let l2 = insert_node(&conn, src_a, None, "link2", "link", 42);
        insert_file_group(&conn, ws, &[l1, l2]);

        let f1 = insert_node(&conn, src_a, None, "copy1.bin", "file", 500);
        let f2 = insert_node(&conn, src_a, None, "copy2.bin", "file", 500);
        insert_file_group(&conn, ws, &[f1, f2]);

        let a = duplicated_size_by_source(&conn, ws)
            .unwrap()
            .get(&src_a)
            .copied()
            .unwrap_or_default();
        assert_eq!(
            a.internal, 500,
            "the surplus real copy counts, the surplus symlink does not"
        );
        assert_eq!(a.cross, 0);
    }

    #[test]
    fn duplicated_size_by_source_ignores_excluded_sources() {
        // The badge used to count redundancy against a device the user had
        // switched off, while the funnel beside it -- which does filter
        // `s.excluded = 0` -- treated the same file as having no partner at
        // all. Two devices disagreeing about one file is what this pins.
        let (conn, ws, src_a, src_b) = setup();
        conn.execute(
            "UPDATE sources SET excluded = 1 WHERE id = ?1",
            params![src_b],
        )
        .unwrap();

        // A's only partner lives on the excluded device, so A's copy is the
        // last one standing: nothing here is surplus.
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);

        let dup = duplicated_size_by_source(&conn, ws).unwrap();
        let a_split = dup.get(&src_a).copied().unwrap_or_default();
        assert_eq!(
            a_split.cross, 0,
            "an excluded device cannot make a file cross-source"
        );
        assert_eq!(
            a_split.internal, 0,
            "one surviving copy is the original, not surplus"
        );
        assert_eq!(
            dup.get(&src_b).copied().unwrap_or_default().total(),
            0,
            "the excluded device reports no duplication of its own"
        );
    }

    #[test]
    fn excluding_a_source_leaves_the_remaining_devices_internal_duplication() {
        // The converse guard: filtering out an excluded device must not also
        // swallow duplication that is entirely internal to a device still in
        // play. Without this, "ignore excluded sources" could be implemented
        // as "ignore any group touching one" and still pass the test above.
        let (conn, ws, src_a, src_b) = setup();
        conn.execute(
            "UPDATE sources SET excluded = 1 WHERE id = ?1",
            params![src_b],
        )
        .unwrap();

        let c1 = insert_node(&conn, src_a, None, "copy1.png", "file", 30);
        let c2 = insert_node(&conn, src_a, None, "copy2.png", "file", 30);
        insert_file_group(&conn, ws, &[c1, c2]);

        let dup = duplicated_size_by_source(&conn, ws).unwrap();
        let a = dup.get(&src_a).copied().unwrap_or_default();
        assert_eq!(a.internal, 30, "A's own surplus copy still counts");
        assert_eq!(a.cross, 0);
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

        let dup = duplicated_size_by_source(&conn, ws)
            .unwrap()
            .get(&src_a)
            .copied()
            .unwrap()
            .total();
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
    fn file_in_two_groups_is_cross_dup_via_either_of_them() {
        // Regression, and the reason the rest of this suite could not catch it:
        // every other test puts a file in exactly one group, which is the case
        // that always worked. Here `a` sits in a cross-source group *and* an
        // internal-only one. The old code kept a single arbitrary group per
        // node and derived each group's source set from that collapsed map, so
        // `a` -- and its partner `b` -- could each resolve to the internal
        // group and read as exclusive-to-this-device despite being a genuine
        // cross-source duplicate. 2,562 files were mislabelled this way.
        let (mut conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        let a2 = insert_node(&conn, src_a, None, "shared_copy.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]); // cross-source
        insert_file_group(&conn, ws, &[a, a2]); // internal to src_a, inserted last

        let cross = load_cross_source_files(&conn, ws).unwrap();
        assert!(cross.contains(&a), "a is cross-source via its first group");
        assert!(cross.contains(&b));
        assert!(
            !cross.contains(&a2),
            "a2 only ever shares a group with a file on its own source"
        );

        build_folder_groups(&mut conn, ws).unwrap();
        rebuild_annotations(&mut conn, ws).unwrap();
        assert_eq!(cross_dup_of(&conn, a), 1, "and the annotation agrees");
        assert_eq!(cross_dup_of(&conn, b), 1);
        assert_eq!(cross_dup_of(&conn, a2), 0);
    }

    #[test]
    fn folder_group_forms_when_files_are_cross_source_only_via_a_second_group() {
        // The same fault in the folder rollup: `dir_dup` under-counted, so a
        // directory could miss the 80%-overlap threshold and produce no folder
        // group at all. Every file here is cross-source, but only through a
        // group that is not the last one inserted for it.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        let a = insert_node(&conn, src_a, Some(dir_a), "one.jpg", "file", 100);
        let b = insert_node(&conn, src_b, Some(dir_b), "one.jpg", "file", 100);
        let a2 = insert_node(&conn, src_a, Some(dir_a), "two.jpg", "file", 100);
        let b2 = insert_node(&conn, src_b, Some(dir_b), "two.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);
        insert_file_group(&conn, ws, &[a2, b2]);
        // Internal-only groups inserted last, so a collapsed node->group map
        // would resolve every file to one of these.
        insert_file_group(&conn, ws, &[a, a2]);
        insert_file_group(&conn, ws, &[b, b2]);

        let groups = build_folder_groups(&mut conn, ws).unwrap();
        assert_eq!(groups, 1, "photos/ is 100% cross-source duplicated");
        rebuild_annotations(&mut conn, ws).unwrap();
        assert_eq!(in_folder_group_of(&conn, dir_a), 1);
    }

    #[test]
    fn hidden_alias_counts_once_in_the_header_and_never_in_its_size() {
        // The device header used to report 5,071 exclusive files where the
        // tree showed 1,322, the 3,749 gap being aliases: the header's stats
        // came from a second walk of `match_members`, which aliases never
        // enter. Both now read `load_cross_source_files`, so they agree.
        let (conn, ws, src_a, src_b) = setup();
        let canonical = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        insert_alias(&conn, src_a, None, "shared_alias.jpg", 100, canonical);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[canonical, other]);

        let stats = cross_dup_size_by_source(&conn, ws).unwrap();
        assert_eq!(
            stats[&src_a].file_count, 2,
            "two names are hidden: the canonical and its alias"
        );
        assert_eq!(
            stats[&src_a].size, 100,
            "but only one file's bytes leave physical_size"
        );
        assert_eq!(
            stats[&src_a].alias_bytes, 100,
            "the alias bytes are reported separately, for the header annotation"
        );
    }

    #[test]
    fn an_empty_directory_is_hidden_by_the_funnel() {
        // `.git/branches` and 152 others: no children at all, so no leaves, so
        // the old `total > 0` guard left them permanently visible showing
        // "0 files". A directory holding nothing holds nothing exclusive.
        let (mut conn, ws, src_a, src_b) = setup();
        let empty = insert_node(&conn, src_a, None, "branches", "directory", 0);
        // Something elsewhere must be duplicated, or the whole pass is a no-op.
        let a = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group(&conn, ws, &[a, b]);

        rebuild_annotations(&mut conn, ws).unwrap();

        assert_eq!(
            cross_dup_of(&conn, empty),
            1,
            "a directory with no leaves is vacuously fully-hidden"
        );
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

    /// A second file beside the `a.jpg` the listing fixtures insert, for tests
    /// that need a folder past the content guard (>= 2 names).
    fn insert_second_file(conn: &Connection, source_id: i64, parent_id: i64) {
        insert_file_with_mtime(
            conn,
            source_id,
            Some(parent_id),
            "b.jpg",
            200,
            Some("2024-01-01_10:00:00"),
        );
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
        // A second file, so the content guard (>= 2 names) is not what
        // decides this test.
        insert_second_file(&conn, src_a, dir_a);
        insert_second_file(&conn, src_b, dir_b);

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
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
        // A second file, so the content guard (>= 2 names) is not what
        // decides this test.
        insert_second_file(&conn, src_a, dir_a);
        insert_second_file(&conn, src_b, dir_b);

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
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

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
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

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
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

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
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
        // A second file, so the content guard (>= 2 names) is not what
        // decides this test.
        insert_second_file(&conn, src_a, dir1);
        insert_second_file(&conn, src_a, dir2);

        let count = compute_listing_hashes(&mut conn, ws, 0).unwrap();
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

        let count = compute_listing_hashes(&mut conn, ws, 0).unwrap();
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
        // A second file, so the content guard (>= 2 names) is not what
        // decides this test.
        insert_second_file(&conn, src_a, year_a);
        insert_second_file(&conn, src_b, year_b);
        // `insert_node` leaves `depth` at 0, but the hash pass visits
        // directories deepest-first by that column, as real imports set it.
        // Without this, `photos` was hashed before `2024` and folded in a
        // zeroed child hash and zeroed totals -- equal on both sources, so
        // the test used to pass without the nesting ever being exercised.
        conn.execute(
            "UPDATE nodes SET depth = 1 WHERE id IN (?1, ?2)",
            params![year_a, year_b],
        )
        .unwrap();

        let count = compute_listing_hashes(&mut conn, ws, 0).unwrap();
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
        // A second file, so the content guard (>= 2 names) is not what
        // decides this test.
        insert_second_file(&conn, src_a, dir_a);
        insert_second_file(&conn, src_b, dir_b);
        conn.execute(
            "UPDATE sources SET excluded = 1 WHERE id = ?1",
            params![src_a],
        )
        .unwrap();

        let count = compute_listing_hashes(&mut conn, ws, 0).unwrap();
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

        compute_listing_hashes(&mut conn, ws, 0).unwrap();
        assert_ne!(
            listing_hash_of(&conn, dir_a),
            listing_hash_of(&conn, dir_b),
            "a directory with a symlink must not hash identically to one without"
        );
    }

    fn insert_file_group_at(conn: &Connection, ws: i64, confidence: f64, member_ids: &[i64]) {
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', ?2, 'test', 0)",
            params![ws, confidence],
        )
        .unwrap();
        let gid = conn.last_insert_rowid();
        for id in member_ids {
            conn.execute(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
                params![gid, id],
            )
            .unwrap();
        }
    }

    /// `(confidence, size)` of the workspace's only group with this signal.
    fn only_group(conn: &Connection, ws: i64, signal: &str) -> (f64, i64) {
        conn.query_row(
            "SELECT confidence, size FROM match_groups
             WHERE workspace_id = ?1 AND primary_signal = ?2",
            params![ws, signal],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn folder_group_confidence_is_byte_weighted_by_the_evidence_beneath_it() {
        // Both folders are 100% duplicated, so the old coverage-based score
        // read 100. But a tenth of the bytes rest on a 45% match, and the
        // confidence must say so: (900*100 + 100*45) / 1000 = 94.5.
        let (mut conn, ws, src_a, src_b) = setup();
        let dir_a = insert_node(&conn, src_a, None, "photos", "directory", 0);
        let dir_b = insert_node(&conn, src_b, None, "photos", "directory", 0);
        let big_a = insert_node(&conn, src_a, Some(dir_a), "big.raw", "file", 900);
        let big_b = insert_node(&conn, src_b, Some(dir_b), "big.raw", "file", 900);
        let small_a = insert_node(&conn, src_a, Some(dir_a), "small.jpg", "file", 100);
        let small_b = insert_node(&conn, src_b, Some(dir_b), "other.jpg", "file", 100);
        insert_file_group_at(&conn, ws, 100.0, &[big_a, big_b]);
        insert_file_group_at(&conn, ws, 45.0, &[small_a, small_b]);

        build_folder_groups(&mut conn, ws).unwrap();
        let (confidence, size) = only_group(&conn, ws, "folder");
        assert!(
            (confidence - 94.5).abs() < 1e-9,
            "expected 94.5, got {confidence}"
        );
        assert_eq!(size, 1000);
    }

    #[test]
    fn cross_source_confidence_is_the_best_cross_source_group_only() {
        // `a` is in three groups: a cross-source one at 45, another
        // cross-source one at 100, and an internal one at 100. Its answer is
        // the best *cross-source* group -- the fixture shape the many-to-many
        // bugs needed, a file in more than one group.
        let (conn, ws, src_a, src_b) = setup();
        let a = insert_node(&conn, src_a, None, "a.jpg", "file", 100);
        let b = insert_node(&conn, src_b, None, "b.jpg", "file", 100);
        let c = insert_node(&conn, src_b, None, "a.jpg", "file", 100);
        let a2 = insert_node(&conn, src_a, None, "a_copy.jpg", "file", 100);
        let d = insert_node(&conn, src_a, None, "d.jpg", "file", 100);
        let e = insert_node(&conn, src_b, None, "e.jpg", "file", 100);
        let d2 = insert_node(&conn, src_a, None, "d_copy.jpg", "file", 100);
        insert_file_group_at(&conn, ws, 45.0, &[a, b]);
        insert_file_group_at(&conn, ws, 100.0, &[a, c]);
        insert_file_group_at(&conn, ws, 100.0, &[a, a2]);
        // `d` is cross-source only at 45; its internal 100 must not lift it.
        insert_file_group_at(&conn, ws, 45.0, &[d, e]);
        insert_file_group_at(&conn, ws, 100.0, &[d, d2]);

        let cross = load_cross_source_files(&conn, ws).unwrap();
        assert_eq!(cross.confidence(a), Some(100.0));
        assert_eq!(cross.confidence(b), Some(45.0));
        assert_eq!(cross.confidence(d), Some(45.0));
        assert_eq!(
            cross.confidence(a2),
            None,
            "internal-only is not cross-source"
        );
    }

    #[test]
    fn an_alias_inherits_its_canonicals_cross_source_confidence() {
        let (conn, ws, src_a, src_b) = setup();
        let canonical = insert_node(&conn, src_a, None, "shared.jpg", "file", 100);
        let alias = insert_alias(&conn, src_a, None, "shared_link.jpg", 100, canonical);
        let other = insert_node(&conn, src_b, None, "shared.jpg", "file", 100);
        insert_file_group_at(&conn, ws, 55.0, &[canonical, other]);

        let cross = load_cross_source_files(&conn, ws).unwrap();
        assert_eq!(cross.confidence(alias), Some(55.0));
    }

    /// Two structurally identical folders on two sources, each holding
    /// `files` (name, size) with one shared mtime.
    fn listing_pair(files: &[(&str, i64)]) -> (Connection, i64) {
        let (conn, ws, src_a, src_b) = setup();
        for src in [src_a, src_b] {
            let dir = insert_node(&conn, src, None, "folder", "directory", 0);
            for (name, size) in files {
                insert_file_with_mtime(
                    &conn,
                    src,
                    Some(dir),
                    name,
                    *size,
                    Some("2024-01-01_10:00:00"),
                );
            }
        }
        (conn, ws)
    }

    #[test]
    fn a_single_file_folder_never_forms_a_listing_group() {
        // Two folders each holding just a `desktop.ini`: structure says
        // nothing here, and at tier-C confidence it would say it loudly.
        let (mut conn, ws) = listing_pair(&[("desktop.ini", 282)]);
        assert_eq!(compute_listing_hashes(&mut conn, ws, 0).unwrap(), 0);
    }

    #[test]
    fn a_listing_group_needs_min_size_bytes_of_content() {
        let (mut conn, ws) = listing_pair(&[("a.jpg", 100), ("b.jpg", 200)]);
        assert_eq!(
            compute_listing_hashes(&mut conn, ws, 301).unwrap(),
            0,
            "300 bytes is under a 301-byte threshold"
        );
        assert_eq!(compute_listing_hashes(&mut conn, ws, 300).unwrap(), 1);
    }

    #[test]
    fn a_listing_group_scores_tier_c_and_is_sized_by_its_subtree() {
        // Sized 0, a listing group was hidden by the default 64 KB filter in
        // the group list; it now carries its subtree's bytes.
        let (mut conn, ws) = listing_pair(&[("a.jpg", 100), ("b.jpg", 200)]);
        compute_listing_hashes(&mut conn, ws, 0).unwrap();
        assert_eq!(only_group(&conn, ws, "listing"), (70.0, 300));
    }
}
