//! The duplicate-matching engine. Matching relies entirely on hashes when
//! available and metadata, structured as absolute vetoes followed by
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
    content_hash: Option<Vec<u8>>,
    hash_kind: Option<String>,
    hash_spec: Option<String>,
}

/// Whether `a` and `b` are both hash-confirmed and have been *proven*
/// different -- the one case where content evidence outright contradicts
/// what a metadata tier would otherwise conclude. Two hashed files under
/// different `hash_spec` values (in principle unreachable within one
/// workspace once locking is enforced, since spec is workspace-wide, but
/// checked here anyway) have nothing comparable and are not "confirmed
/// different" by this definition -- they simply fall through to ordinary
/// metadata evidence.
fn hash_confirmed_different(a: &FileRow, b: &FileRow) -> bool {
    match (
        a.content_hash.as_ref(),
        b.content_hash.as_ref(),
        a.hash_spec.as_ref(),
        b.hash_spec.as_ref(),
    ) {
        (Some(ha), Some(hb), Some(sa), Some(sb)) => sa == sb && ha != hb,
        _ => false,
    }
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
/// Tiers A and B are the one place transitive union-find-style grouping is
/// actually valid: exact digest equality under a shared hash spec is a true
/// equivalence relation, unlike any metadata signal below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Full-content BLAKE3 match under a shared hash spec. Confirmed.
    A,
    /// Sampled-content BLAKE3 match under a shared hash spec. Confirmed
    /// (sampling's residual risk is informational, not a caveat).
    B,
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
            Tier::A => 100.0,
            Tier::B => 99.0,
            Tier::C => 70.0,
            Tier::D => 55.0,
            Tier::E => 45.0,
        }
    }
    fn signal(self) -> &'static str {
        match self {
            Tier::A => "hash-full",
            Tier::B => "hash-sampled",
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
/// expensive. This bound is sized around *quadratic* work (the tier C/D
/// exact-name promotion check) -- it is deliberately NOT reused for tier E's
/// clique search below, which has a completely different cost shape.
const BUCKET_SAFETY_CAP: usize = 2000;

/// Cap on how many same-(size, extension, parent-folder) candidates tier E's
/// clique search will even attempt. Unlike `BUCKET_SAFETY_CAP`, this bounds a
/// step whose cost is combinatorial, not quadratic: enumerating every
/// maximal clique in a graph is worst-case exponential in vertex count (the
/// Moon-Moser bound: a dense n-vertex graph can have up to 3^(n/3) maximal
/// cliques), so `BUCKET_SAFETY_CAP` itself is nowhere near safe here. Real
/// archives routinely produce dense graphs at this step -- many cameras and
/// phones all use the same folder name (`DCIM`), so every same-size,
/// same-extension file across every such folder in the workspace lands in
/// one cluster, and if their mtimes also cluster within the tolerance
/// windows, most pairs pass the edge test and the graph is close to
/// complete, which is exactly where clique enumeration is most expensive.
const MAX_CLIQUE_CANDIDATES: usize = 300;

/// Hard ceiling on Bron-Kerbosch recursive calls, independent of
/// `MAX_CLIQUE_CANDIDATES`: vertex count alone doesn't bound the cost of a
/// dense graph, since even a few hundred fully-connected vertices can take
/// arbitrarily long to fully enumerate. Exceeding the budget aborts the
/// search for that one cluster -- treated exactly like an oversized group
/// (see `MAX_GROUP_SIZE`): no tier-E groups are persisted from it, which
/// only costs disk space, never a false positive.
const CLIQUE_WORK_BUDGET: u32 = 200_000;

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
/// via the simplest (no-pivot) form of Bron-Kerbosch, or `None` if
/// `CLIQUE_WORK_BUDGET` is exhausted first -- vertex count bounds the size
/// of the adjacency structure but not the cost of enumerating cliques over
/// it, so a dense graph can still exceed the budget well under
/// `MAX_CLIQUE_CANDIDATES` vertices. Callers must treat `None` exactly like
/// an oversized group: skip it, don't persist anything from it.
fn maximal_cliques(
    vertices: &[usize],
    edge: impl Fn(usize, usize) -> bool,
    budget: u32,
) -> Option<Vec<Vec<usize>>> {
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
    let mut budget = budget;
    bron_kerbosch(
        &mut Vec::new(),
        p,
        BTreeSet::new(),
        &adj,
        &mut cliques,
        &mut budget,
    )?;
    Some(cliques)
}

/// Returns `None` as soon as `budget` runs out, unwinding the whole search
/// (via `?` at each recursive call) rather than returning a partial result:
/// which subset of the true maximal cliques gets found first depends on
/// `BTreeSet` iteration order, so a partial enumeration would be an
/// arbitrary, non-obvious sample rather than a meaningful one -- dropping
/// the whole cluster is simpler to reason about and consistent with this
/// matcher's "when ambiguous or expensive to verify, drop rather than guess"
/// design.
fn bron_kerbosch(
    r: &mut Vec<usize>,
    mut p: BTreeSet<usize>,
    mut x: BTreeSet<usize>,
    adj: &HashMap<usize, BTreeSet<usize>>,
    cliques: &mut Vec<Vec<usize>>,
    budget: &mut u32,
) -> Option<()> {
    *budget = budget.checked_sub(1)?;
    if p.is_empty() && x.is_empty() {
        if r.len() >= 2 {
            cliques.push(r.clone());
        }
        return Some(());
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
            budget,
        )?;
        r.pop();
        p.remove(&v);
        x.insert(v);
    }
    Some(())
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
///
/// Two hash-derived exclusions apply on top of that, and they are opposites
/// of each other -- one for hash-confirmed-*different* members, one for
/// hash-confirmed-*same* ones:
///
/// 1. **The hash veto** (both hashed, same spec, digests differ): a
///    hash-confirmed-different pair must never be softened into a metadata
///    match. Tier E folds this into its edge condition directly. Tier C/D's
///    exact-name grouping has no per-pair check to fold it into (name
///    equality is transitive, which is what makes it clique-free in the
///    first place) -- so instead, a name-group that internally contains two
///    hash-confirmed-different members is dropped entirely rather than
///    partially reconciled, consistent with this matcher's "when a group is
///    internally contradictory or expensive to verify, drop rather than
///    guess" design (the same rule `MAX_GROUP_SIZE` and the clique-budget
///    cutoff already apply elsewhere).
/// 2. **The redundancy rule** (`group_is_hash_redundant`): a metadata group
///    whose members *all* belong to one and the same tier A/B hash group is
///    dropped, because it only restates -- at lower confidence, on weaker
///    evidence -- a group `run_with_progress` already formed from content
///    itself. Without this, a hashed corpus produces two groups for nearly
///    every duplicate set and roughly doubles the count the UI reports.
///
/// The redundancy rule is deliberately whole-group rather than per-pair.
/// Suppressing individual pairs would break a *mixed* group (some members
/// hash-mates, some not) into one group per cross-cohort pairing, which
/// inflates the group count in exactly the way this rule exists to prevent.
/// So a mixed group is emitted intact, hash-mates and all; only the wholly
/// redundant case is dropped.
fn resolve_groups_in_bucket(
    files: &[FileRow],
    hash_group_of: &[Option<usize>],
    idxs: &[usize],
) -> Vec<(Tier, Vec<usize>)> {
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
            if group_has_hash_conflict(files, &group) {
                continue;
            }
            if group_is_hash_redundant(hash_group_of, &group) {
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
            // See `MAX_CLIQUE_CANDIDATES`: this is deliberately much tighter
            // than `BUCKET_SAFETY_CAP` above, since clique enumeration's
            // cost is combinatorial, not quadratic.
            if cluster.len() < 2 || cluster.len() > MAX_CLIQUE_CANDIDATES {
                continue;
            }
            let Some(cliques) = maximal_cliques(
                &cluster,
                |a, b| {
                    !hash_confirmed_different(&files[a], &files[b])
                        && files[a].name != files[b].name
                        && compare_mtime(files[a].mtime.as_deref(), files[b].mtime.as_deref())
                            != MtimeMatch::None
                },
                CLIQUE_WORK_BUDGET,
            ) else {
                // Work budget exhausted: this cluster's edge graph is too
                // dense to safely enumerate. Drop it, same as an oversized
                // group -- not a false positive, just a missed one.
                continue;
            };
            for clique in cliques {
                if clique.len() > MAX_GROUP_SIZE {
                    continue;
                }
                // Applied to the emitted clique rather than to the edge
                // condition above: as an edge rule it would split mixed
                // cliques into several smaller groups, the outcome the
                // whole-group framing exists to avoid.
                if group_is_hash_redundant(hash_group_of, &clique) {
                    continue;
                }
                out.push((Tier::E, clique));
            }
        }
    }

    out
}

/// Whether `group` contains two members that are hash-confirmed different
/// from each other -- see `resolve_groups_in_bucket`'s doc comment on why
/// this drops the whole tier C/D name-group rather than trying to partially
/// reconcile it.
fn group_has_hash_conflict(files: &[FileRow], group: &[usize]) -> bool {
    for i in 0..group.len() {
        for j in (i + 1)..group.len() {
            if hash_confirmed_different(&files[group[i]], &files[group[j]]) {
                return true;
            }
        }
    }
    false
}

/// Whether every member of `group` belongs to the same tier A/B hash group,
/// making this metadata group a pure restatement of one already formed from
/// content evidence -- see `resolve_groups_in_bucket`'s doc comment.
///
/// A single `Vec<Option<usize>>` lookup suffices because a file's hash-group
/// key is `(size, hash_spec, content_hash)`, which is a function of the file:
/// a file belongs to at most one hash group, so "same group" needs no set
/// intersection. An unhashed member (`None`), or two members in *different*
/// hash groups, means the group carries evidence no single hash group
/// already covers, and it survives.
fn group_is_hash_redundant(hash_group_of: &[Option<usize>], group: &[usize]) -> bool {
    let Some(&first) = group.first() else {
        return false;
    };
    let Some(id) = hash_group_of[first] else {
        return false;
    };
    group.iter().all(|&i| hash_group_of[i] == Some(id))
}

/// Run the full dedup pass for a workspace: clears prior groups, rebuilds file
/// groups, then folder-level groups, all inside one transaction.
///
/// The extension veto is applied by sub-bucketing on it (see
/// `resolve_groups_in_bucket`) and scoped to the metadata tiers (C-E) only --
/// tiers A and B rest on content evidence, which supersedes every metadata
/// veto except the structural exclusions (alias, symlink) applied upstream in
/// `load_files` (§9.1 of the design spec: "Vetoes apply to the metadata tiers
/// (C-E). Tiers A and B rest on content evidence, which supersedes every
/// metadata veto except the structural exclusions (alias, symlink) applied
/// upstream in load_files."). Identical bytes are identical bytes: the
/// extension veto exists to stop metadata coincidence (same size, same stem,
/// different container), and that reasoning has no force once content is
/// verified -- a `clip.mov` / `clip.mp4` pair with the same digest is a true
/// duplicate the user should see. A file's own veto conditions -- being a
/// hardlink alias or a symlink -- never apply here at all, because
/// `load_files` excludes both from the candidate pool entirely (aliases via
/// `alias_of IS NULL`, symlinks via `type = 'file'`). A file may legitimately
/// end up in more than one persisted group across tiers (e.g. a tier-D match
/// on exact name with one partner, and a separate tier-E match on parent
/// folder with a different partner) -- this reflects two independent pieces
/// of partial evidence, not a bug; `get_group_for_node` already resolves
/// "which one to show" via `ORDER BY confidence DESC LIMIT 1`. What is *not*
/// legitimate is the degenerate version of that: a metadata group with the
/// same membership as a hash group, which is one piece of evidence counted
/// twice. `group_is_hash_redundant` drops those, so the group count the UI
/// reports stays proportional to the number of distinct duplicate sets found
/// rather than doubling as soon as a workspace is hashed.
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

    on_phase("matching", 1, TOTAL_PHASES);
    let mut groups: Vec<(Tier, Vec<usize>)> = Vec::new();

    // Tiers A/B: hash-confirmed identity is a true equivalence relation
    // (§9.3), so this is a single O(n) grouping pass by exact
    // (size, hash_spec, digest) -- no pairwise scoring, no clique search.
    // A group promotes to A only if every member was *fully* hashed; any
    // sampled member downgrades the whole group to B, since sampling is the
    // weaker of the two forms of content evidence actually present.
    let mut by_hash: HashMap<(i64, String, Vec<u8>), Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if let (Some(hash), Some(spec)) = (&f.content_hash, &f.hash_spec) {
            by_hash
                .entry((f.size, spec.clone(), hash.clone()))
                .or_default()
                .push(i);
        }
    }
    // Which hash group each file landed in, so the metadata tiers below can
    // drop groups that merely restate one of these (see
    // `group_is_hash_redundant`). `None` for a file in no hash group.
    let mut hash_group_of: Vec<Option<usize>> = vec![None; files.len()];
    for group in by_hash.into_values() {
        // No MAX_GROUP_SIZE cap here, unlike the metadata tiers below: the cap
        // exists because a large metadata cluster is probably systematic
        // noise. Hash-confirmed identity is proof, not noise -- a file
        // copied to 40 discs is ordinary in this corpus, and dropping that
        // group would be exactly the silent-loss failure the cap was meant
        // to prevent elsewhere.
        if group.len() < 2 {
            continue;
        }
        let tier = if group
            .iter()
            .all(|&i| files[i].hash_kind.as_deref() == Some("full"))
        {
            Tier::A
        } else {
            Tier::B
        };
        // Only a hash group that will actually be *persisted* may suppress a
        // metadata group: suppressing on behalf of a group the insert loop
        // below then drops would erase the match from the UI entirely rather
        // than merely de-duplicate it.
        //
        // With today's constants this can't happen -- every hash tier (100,
        // 99) outranks every metadata tier (70, 55, 45), so any
        // `min_confidence` that drops a hash group drops all its possible
        // metadata counterparts too. The check is kept anyway so the
        // suppression rule is self-contained, rather than silently correct
        // only because of a numeric coincidence between two separate sets of
        // tier constants that a future tier could break.
        if tier.confidence() >= params.min_confidence {
            let id = groups.len();
            for &i in &group {
                hash_group_of[i] = Some(id);
            }
        }
        groups.push((tier, group));
    }

    // Metadata tiers (C/D/E) run over ALL files -- hashed and unhashed
    // alike, since a hashed file paired with an *unhashed* one is still
    // eligible for ordinary metadata evidence. Two exclusions apply (see
    // `resolve_groups_in_bucket`'s doc comment): two hashed-and-*different*
    // files must never co-occur in a metadata group, and a group whose
    // members all sit in one hash group is dropped as redundant.
    //
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
    for idxs in by_size.values() {
        if idxs.len() < 2 {
            continue;
        }
        groups.extend(resolve_groups_in_bucket(&files, &hash_group_of, idxs));
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
    // Sweep hardlink groups orphaned by any route (source deletion, alias
    // rows removed, etc.) -- including down to a single surviving member,
    // which is meaningless for a "these names are the same file" group.
    // Doing this at the start of every dedup pass rather than at
    // source-deletion time keeps the cleanup in one place and self-healing.
    tx.execute(
        "DELETE FROM match_groups
         WHERE workspace_id = ?1 AND kind = 'hardlink'
           AND (SELECT COUNT(*) FROM match_members mm WHERE mm.group_id = match_groups.id) < 2",
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

    // Folder-level rollup uses the freshly written file groups; the listing-
    // hash pass is independent of file groups entirely (pure structure) but
    // shares this phase since both produce `kind='folder'` groups.
    //
    // `build_folder_groups` and `rebuild_annotations` each need the same full
    // `nodes` scan (every file's location, every directory's parent), so load
    // it once here and pass it to both `_with` variants instead of letting
    // each of them independently re-query the whole table.
    on_phase("folder_rollup", 3, TOTAL_PHASES);
    let rollup_files = super::rollup::load_file_locs(conn, workspace_id)?;
    let rollup_parent_of = super::rollup::load_parent_of(conn, workspace_id)?;
    let folder_groups = super::rollup::build_folder_groups_with(
        conn,
        workspace_id,
        &rollup_files,
        &rollup_parent_of,
    )?;
    let listing_groups = super::rollup::compute_listing_hashes(conn, workspace_id)?;

    // Rebuild the per-node duplicate annotation cache so tree browsing is fast.
    on_phase("annotating", 4, TOTAL_PHASES);
    super::rollup::rebuild_annotations_with(conn, workspace_id, &rollup_files, &rollup_parent_of)?;

    on_phase("done", TOTAL_PHASES, TOTAL_PHASES);
    Ok(group_count + folder_groups + listing_groups)
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
        "SELECT n.id, n.parent_id, n.name, n.size, n.mtime, n.content_hash, n.hash_kind, n.hash_spec
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
            content_hash: r.get(5)?,
            hash_kind: r.get(6)?,
            hash_spec: r.get(7)?,
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

    /// Set the hash columns on the (single, since `setup_files` never
    /// creates two same-name files under the same parent) node named `name`
    /// under `parent_name` (empty for a top-level node).
    fn set_hash(
        conn: &Connection,
        name: &str,
        parent_name: &str,
        hash_kind: &str,
        hash_spec: &str,
        digest: &[u8],
    ) {
        if parent_name.is_empty() {
            conn.execute(
                "UPDATE nodes SET content_hash = ?1, hash_kind = ?2, hash_spec = ?3
                 WHERE name = ?4 AND parent_id IS NULL",
                params![digest, hash_kind, hash_spec, name],
            )
            .unwrap();
        } else {
            conn.execute(
                "UPDATE nodes SET content_hash = ?1, hash_kind = ?2, hash_spec = ?3
                 WHERE name = ?4 AND parent_id = (SELECT id FROM nodes WHERE name = ?5)",
                params![digest, hash_kind, hash_spec, name, parent_name],
            )
            .unwrap();
        }
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

        // Two independent folder-level detectors both fire on this fixture
        // (the two "photos" directories are byte-identical): the 80%-dup
        // byte heuristic in `build_folder_groups`, and the structural
        // listing-hash pass in `compute_listing_hashes`. Both firing is
        // correct, not a regression -- they are deliberately independent
        // mechanisms (see `rollup::compute_listing_hashes`'s doc comment).
        let folder_signals: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT primary_signal FROM match_groups WHERE workspace_id = ?1 AND kind = 'folder' ORDER BY primary_signal",
                )
                .unwrap();
            stmt.query_map(params![ws], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            folder_signals,
            vec!["folder".to_string(), "listing".to_string()],
            "the photos folder should be flagged by both the byte-overlap heuristic and the listing hash"
        );
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
        // With an explicitly-lowered 4 KB threshold (the default is 64 KB) + high min confidence, tiny files drop out.
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
    fn full_hash_match_forms_tier_a() {
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, None, ""),
            ("b.jpg", 500_000, None, ""), // different name/no mtime -- pure hash evidence
        ]);
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xAA; 32]);
        set_hash(&conn, "b.jpg", "", "full", "blake3/v1/full", &[0xAA; 32]);

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![(100.0, "hash-full".to_string())]
        );
    }

    #[test]
    fn sampled_hash_match_forms_tier_b() {
        let mut conn = setup_files(&[("a.jpg", 500_000, None, ""), ("b.jpg", 500_000, None, "")]);
        set_hash(
            &conn,
            "a.jpg",
            "",
            "full",
            "blake3/v1/th1-s1-t1",
            &[0xBB; 32],
        );
        set_hash(
            &conn,
            "b.jpg",
            "",
            "sampled",
            "blake3/v1/th1-s1-t1",
            &[0xBB; 32],
        );

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![(99.0, "hash-sampled".to_string())],
            "any sampled member downgrades the whole group from A to B"
        );
    }

    #[test]
    fn hashed_and_different_pair_never_falls_back_to_metadata_match() {
        // Exact name + strong mtime -- everything tier C would normally
        // reward -- but hash-confirmed different under the same spec.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
        ]);
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xCC; 32]);
        // set_hash matches by name alone, so it would hit both same-named
        // rows; give the second one a distinct digest via its node id instead.
        let ws = 1;
        conn.execute(
            "UPDATE nodes SET content_hash = ?1, hash_kind = 'full', hash_spec = 'blake3/v1/full'
             WHERE type = 'file' AND id = (SELECT MAX(id) FROM nodes WHERE type = 'file')",
            params![vec![0xDD_u8; 32]],
        )
        .unwrap();

        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert!(
            group_rows(&conn, ws).is_empty(),
            "a hash-confirmed-different pair must never form any group, hash or metadata"
        );
    }

    #[test]
    fn hashed_vs_unhashed_pair_still_forms_metadata_tier_on_real_evidence() {
        // One file hashed, the other never scanned/hashed (e.g. a JSON
        // import) -- there's no digest to compare, so ordinary name+mtime
        // evidence is still legitimate and must not be suppressed.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
        ]);
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xEE; 32]);
        // set_hash updates every row named "a.jpg"; clear it back off the
        // second one so only one of the pair is actually hashed.
        conn.execute(
            "UPDATE nodes SET content_hash = NULL, hash_kind = NULL, hash_spec = NULL
             WHERE type = 'file' AND id = (SELECT MAX(id) FROM nodes WHERE type = 'file')",
            [],
        )
        .unwrap();

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![(70.0, "name+mtime".to_string())],
            "a hashed file must still match an unhashed one on real metadata evidence"
        );
    }

    #[test]
    fn differing_hash_spec_does_not_collide() {
        // Identical digest bytes but different spec strings -- must never be
        // treated as the same content (a defensive/pathological case; spec
        // locking should make this unreachable in practice within a
        // workspace, but the matcher must not rely on that alone).
        let mut conn = setup_files(&[("a.jpg", 500_000, None, ""), ("b.jpg", 500_000, None, "")]);
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xFF; 32]);
        set_hash(
            &conn,
            "b.jpg",
            "",
            "full",
            "blake3/v1/th1-s1-t1",
            &[0xFF; 32],
        );

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert!(group_rows(&conn, ws).is_empty());
    }

    #[test]
    fn hash_group_is_not_restated_as_metadata_group() {
        // Two files that hash identically AND share name/size/mtime. The
        // hash group already says everything the name+mtime group would, so
        // only the tier-A group may be persisted -- otherwise every hashed
        // duplicate set is listed twice and the UI's group count doubles.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
        ]);
        // Both rows are named "a.jpg" at top level, so this hashes both.
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xAB; 32]);

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![(100.0, "hash-full".to_string())],
            "a metadata group with the same members as a hash group is one \
             piece of evidence counted twice and must not be persisted"
        );
    }

    #[test]
    fn hashed_pair_still_matches_an_unhashed_third_file() {
        // Mixed group: `a`/`b` are hash-mates, `c` was never hashed. The
        // metadata group is NOT wholly redundant -- it carries the a-c and
        // b-c evidence no hash group covers -- so it survives intact, with
        // all three members. Deliberately whole-group rather than per-pair:
        // suppressing just the a-b pair would split this into {a,c} and
        // {b,c}, inflating the count instead of reducing it.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), ""),
        ]);
        set_hash(&conn, "a.jpg", "", "full", "blake3/v1/full", &[0xAB; 32]);
        // set_hash hit all three rows; unhash the last so only two are mates.
        conn.execute(
            "UPDATE nodes SET content_hash = NULL, hash_kind = NULL, hash_spec = NULL
             WHERE type = 'file' AND id = (SELECT MAX(id) FROM nodes WHERE type = 'file')",
            [],
        )
        .unwrap();

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![
                (100.0, "hash-full".to_string()),
                (70.0, "name+mtime".into())
            ],
            "a mixed group carries evidence no hash group covers and must survive"
        );

        let members: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_members mm
                 JOIN match_groups mg ON mg.id = mm.group_id
                 WHERE mg.workspace_id = ?1 AND mg.primary_signal = 'name+mtime'",
                params![ws],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            members, 3,
            "the surviving metadata group keeps all three members, hash-mates included"
        );
    }

    #[test]
    fn tier_e_clique_wholly_inside_one_hash_group_is_dropped() {
        // Tier E's exclusion is applied to the emitted clique, not to the
        // edge condition. Two hash-mates with differing names under a shared
        // parent would otherwise form a redundant parent+mtime group.
        let mut conn = setup_files(&[
            ("a.jpg", 500_000, Some("2024-01-01_10:00:00"), "p"),
            ("b.jpg", 500_000, Some("2024-01-01_10:00:00"), "p"),
        ]);
        set_hash(&conn, "a.jpg", "p", "full", "blake3/v1/full", &[0xAB; 32]);
        set_hash(&conn, "b.jpg", "p", "full", "blake3/v1/full", &[0xAB; 32]);

        let ws = 1;
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        assert_eq!(
            group_rows(&conn, ws),
            vec![(100.0, "hash-full".to_string())],
            "tier E must not restate a hash group either"
        );
    }

    #[test]
    fn hash_redundancy_needs_one_shared_group_not_merely_hashed_members() {
        // The rule keys on shared hash-group membership, not on "is hashed".
        assert!(group_is_hash_redundant(&[Some(0), Some(0)], &[0, 1]));
        assert!(
            !group_is_hash_redundant(&[Some(0), Some(1)], &[0, 1]),
            "members of two different hash groups are not covered by either"
        );
        assert!(
            !group_is_hash_redundant(&[Some(0), None], &[0, 1]),
            "an unhashed member means the group carries uncovered evidence"
        );
        assert!(!group_is_hash_redundant(&[None, None], &[0, 1]));
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

    #[test]
    fn clique_search_aborts_when_work_budget_is_exhausted() {
        // A complete graph on 4 vertices has exactly one maximal clique but
        // still takes more than a single recursive call to find -- an
        // artificially tiny budget must abort rather than return a wrong or
        // partial answer.
        let vertices = vec![0usize, 1, 2, 3];
        let result = maximal_cliques(&vertices, |_, _| true, 1);
        assert!(
            result.is_none(),
            "a budget of 1 must not be enough to finish even a 4-vertex complete graph"
        );
    }

    #[test]
    fn clique_search_finds_the_clique_when_budget_is_sufficient() {
        let vertices = vec![0usize, 1, 2, 3];
        let result = maximal_cliques(&vertices, |_, _| true, CLIQUE_WORK_BUDGET).unwrap();
        assert_eq!(result, vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn oversized_dense_parent_cluster_is_skipped_quickly() {
        // A cluster larger than MAX_CLIQUE_CANDIDATES, with every mtime
        // identical (so the edge graph would be complete -- the shape real
        // archives produce when many devices share a folder name like
        // "DCIM" and photos cluster in time) must be skipped outright
        // rather than attempt clique enumeration at all.
        let mut rows: Vec<(String, i64, Option<String>, String)> = Vec::new();
        for i in 0..(MAX_CLIQUE_CANDIDATES + 50) {
            rows.push((
                format!("file{i}.jpg"),
                123_456,
                Some("2024-01-01_10:00:00".to_string()),
                "DCIM".to_string(),
            ));
        }
        let borrowed: Vec<(&str, i64, Option<&str>, &str)> = rows
            .iter()
            .map(|(n, s, m, p)| (n.as_str(), *s, m.as_deref(), p.as_str()))
            .collect();
        let mut conn = setup_files(&borrowed);
        let ws = 1;

        let start = std::time::Instant::now();
        run_with_progress(&mut conn, ws, DedupParams::default(), |_, _, _| {}).unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "an oversized dense parent cluster must be skipped quickly, took {elapsed:?}"
        );
        assert!(
            group_rows(&conn, ws).is_empty(),
            "no groups should form: names all differ (no tier C/D match) and \
             the parent cluster exceeds MAX_CLIQUE_CANDIDATES (dropped, not analyzed)"
        );
    }
}
