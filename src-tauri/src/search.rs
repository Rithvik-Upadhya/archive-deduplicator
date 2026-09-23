//! Name search for the top-bar search box: a substring match on a node's
//! *name* (never its path), optionally case-sensitive. `*` (any run of
//! characters, including none) and `?` (exactly one character) are
//! wildcards inside that substring match -- unanchored, so `IMG_*.jpg` also
//! matches `old-IMG_1.jpg.bak`. There is no escape for a literal `*`/`?`;
//! both are illegal in Windows names and rare elsewhere.
//!
//! The comparison lives in Rust and is exposed to SQL as `search_match`,
//! because SQLite's own `lower()` and `LIKE` fold ASCII only -- an archive of
//! accented names would then match in one case and not the other. Rust's
//! `to_lowercase` and JS's `toLowerCase` both apply full Unicode default case
//! mapping, and `?` consumes one `char` here as `.` consumes one code point
//! under JS's `u` flag, so the backend filter and the frontend
//! (`util.compileNameQuery`, which both the highlight and the consolidated
//! tree's filter use) agree on what matched. Change one, change both.

use crate::model::NodeSearchResult;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, params};

/// The needle as the comparison expects it: lowercased unless the search is
/// case-sensitive. Folded once per query rather than once per row.
pub fn fold_needle(needle: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        needle.to_string()
    } else {
        needle.to_lowercase()
    }
}

/// Whether `name` contains an already-folded `needle` (see `fold_needle`),
/// with `*`/`?` in the needle acting as wildcards.
fn contains_folded(name: &str, folded: &str, case_sensitive: bool) -> bool {
    let has_wildcard = folded.contains(['*', '?']);
    match (has_wildcard, case_sensitive) {
        // The fast path: this runs once per node in the workspace.
        (false, true) => name.contains(folded),
        (false, false) => name.to_lowercase().contains(folded),
        (true, true) => glob_contains(name, folded),
        (true, false) => glob_contains(&name.to_lowercase(), folded),
    }
}

/// Unanchored glob: whether some substring of `name` matches `pattern`,
/// i.e. an anchored match of `*pattern*`. Works over `char`s, so `?` takes
/// one Unicode scalar value, never a partial UTF-8 sequence.
fn glob_contains(name: &str, pattern: &str) -> bool {
    let s: Vec<char> = name.chars().collect();
    let p: Vec<char> = std::iter::once('*')
        .chain(pattern.chars())
        .chain(std::iter::once('*'))
        .collect();
    glob_match(&s, &p)
}

/// Anchored `*`/`?` match: the standard two-pointer algorithm, backtracking
/// only to the most recent `*`, which suffices because a later `*` can absorb
/// anything an earlier one would have. Linear in the common case.
fn glob_match(s: &[char], p: &[char]) -> bool {
    let (mut si, mut pi) = (0, 0);
    // The last `*` seen, and the position in `s` it has absorbed up to.
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if let Some((star_pi, star_si)) = star {
            // Let that `*` absorb one more character and retry after it.
            star = Some((star_pi, star_si + 1));
            pi = star_pi + 1;
            si = star_si + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Whether `name` contains `needle` (wildcards included), honouring
/// `case_sensitive` -- the whole
/// comparison in one call, as SQL sees it through `fold_needle` +
/// `search_match`. Production code always goes through SQL, so this exists
/// for the tests that pin the folding behaviour.
#[cfg(test)]
fn name_matches(name: &str, needle: &str, case_sensitive: bool) -> bool {
    contains_folded(name, &fold_needle(needle, case_sensitive), case_sensitive)
}

/// Register `search_match(name, folded_needle, case_sensitive)` on `conn`.
/// Cheap and idempotent (re-registering replaces), so every caller registers
/// it itself instead of relying on how its connection was opened.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "search_match",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let name: Option<String> = ctx.get(0)?;
            let needle: String = ctx.get(1)?;
            let case_sensitive: bool = ctx.get(2)?;
            Ok(name.is_some_and(|n| contains_folded(&n, &needle, case_sensitive)))
        },
    )
}

/// Every node on a live (non-excluded) source of the workspace whose name
/// matches, plus every strict ancestor of one -- the rows a filtered device
/// tree must keep, and the folders to open to reach them.
pub fn search_nodes(
    conn: &Connection,
    workspace_id: i64,
    query: &str,
    case_sensitive: bool,
) -> rusqlite::Result<NodeSearchResult> {
    register(conn)?;
    let needle = fold_needle(query, case_sensitive);
    let mut matched_stmt = conn.prepare(
        "SELECT n.id FROM nodes n JOIN sources s ON s.id = n.source_id
         WHERE s.workspace_id = ?1 AND s.excluded = 0
           AND search_match(n.name, ?2, ?3)",
    )?;
    let matched = matched_stmt
        .query_map(params![workspace_id, needle, case_sensitive], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;

    // Seeded from the hits' parents, so a hit is an ancestor only when it
    // really sits above another hit. `UNION` (not `UNION ALL`) drops a folder
    // reached from several hits, which also bounds the recursion.
    let mut anc_stmt = conn.prepare(
        "WITH RECURSIVE anc(id, parent_id) AS (
             SELECT p.id, p.parent_id
             FROM nodes h
             JOIN sources s ON s.id = h.source_id
             JOIN nodes p ON p.id = h.parent_id
             WHERE s.workspace_id = ?1 AND s.excluded = 0
               AND search_match(h.name, ?2, ?3)
             UNION
             SELECT p.id, p.parent_id FROM nodes p JOIN anc a ON p.id = a.parent_id
         )
         SELECT id FROM anc",
    )?;
    let ancestors = anc_stmt
        .query_map(params![workspace_id, needle, case_sensitive], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;

    Ok(NodeSearchResult { matched, ancestors })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with_workspace() -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();
        (conn, ws)
    }

    fn insert_source(conn: &Connection, ws: i64, excluded: bool) -> i64 {
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count, excluded)
             VALUES (?1, 'scan', 'd', 'd', 't', 0, 0, ?2)",
            params![ws, excluded as i64],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_node(
        conn: &Connection,
        source_id: i64,
        parent_id: Option<i64>,
        name: &str,
        node_type: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
             VALUES (?1, ?2, ?3, ?3, ?4, 10)",
            params![source_id, parent_id, name, node_type],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn name_matches_folds_non_ascii_case_only_when_insensitive() {
        assert!(name_matches("Été 2019.jpg", "été", false));
        assert!(name_matches("été 2019.jpg", "ÉTÉ", false));
        assert!(!name_matches("Été 2019.jpg", "été", true));
        assert!(name_matches("Été 2019.jpg", "Été", true));
        assert!(!name_matches("photo.jpg", "video", false));
    }

    #[test]
    fn name_matches_treats_star_and_question_mark_as_unanchored_wildcards() {
        assert!(name_matches("IMG_0042.jpg", "IMG_*.jpg", true));
        assert!(
            name_matches("old-IMG_1.jpg.bak", "IMG_*.jpg", true),
            "unanchored: the pattern may sit anywhere in the name"
        );
        assert!(!name_matches("IMG_0042.png", "IMG_*.jpg", true));

        assert!(name_matches("report 1.pdf", "report ?.pdf", true));
        assert!(!name_matches("report 12.pdf", "report ?.pdf", true));
        assert!(
            !name_matches("report .pdf", "report ?.pdf", true),
            "`?` needs exactly one character"
        );

        assert!(
            name_matches("résumé.doc", "r?sum?", true),
            "`?` consumes one whole non-ASCII character"
        );
        assert!(name_matches("IMG_1.JPG", "img_*", false));
        assert!(!name_matches("IMG_1.JPG", "img_*", true));

        assert!(name_matches("anything", "*", true));
        assert!(name_matches("", "*", true));
        assert!(name_matches("abc", "a**c", true));
        assert!(!name_matches("abc", "a**d", true));
        // Needs backtracking: the first `b` is not the one that leads to `c`.
        assert!(name_matches("abxbc", "a*bc", true));
    }

    #[test]
    fn search_nodes_returns_nested_hits_with_every_ancestor() {
        let (conn, ws) = conn_with_workspace();
        let src = insert_source(&conn, ws, false);
        let root = insert_node(&conn, src, None, "Photos", "directory");
        let year = insert_node(&conn, src, Some(root), "2019", "directory");
        let hit = insert_node(&conn, src, Some(year), "Été.jpg", "file");
        let _miss = insert_node(&conn, src, Some(year), "winter.jpg", "file");
        let _other_root = insert_node(&conn, src, None, "Music", "directory");

        let res = search_nodes(&conn, ws, "été", false).unwrap();
        assert_eq!(res.matched, vec![hit]);
        let mut anc = res.ancestors.clone();
        anc.sort();
        assert_eq!(anc, vec![root, year]);

        assert!(
            search_nodes(&conn, ws, "été", true)
                .unwrap()
                .matched
                .is_empty()
        );
    }

    #[test]
    fn search_nodes_applies_wildcards_through_sql() {
        let (conn, ws) = conn_with_workspace();
        let src = insert_source(&conn, ws, false);
        let dir = insert_node(&conn, src, None, "DCIM", "directory");
        let hit = insert_node(&conn, src, Some(dir), "IMG_0042.JPG", "file");
        insert_node(&conn, src, Some(dir), "IMG_0042.PNG", "file");

        let res = search_nodes(&conn, ws, "img_*.jpg", false).unwrap();
        assert_eq!(res.matched, vec![hit]);
        assert_eq!(res.ancestors, vec![dir]);
    }

    #[test]
    fn search_nodes_reports_a_hit_that_is_also_an_ancestor_in_both_lists() {
        let (conn, ws) = conn_with_workspace();
        let src = insert_source(&conn, ws, false);
        let dir = insert_node(&conn, src, None, "report", "directory");
        let file = insert_node(&conn, src, Some(dir), "report.pdf", "file");

        let res = search_nodes(&conn, ws, "report", false).unwrap();
        let mut matched = res.matched.clone();
        matched.sort();
        assert_eq!(matched, vec![dir, file]);
        assert_eq!(res.ancestors, vec![dir]);
    }

    #[test]
    fn search_nodes_ignores_excluded_sources() {
        let (conn, ws) = conn_with_workspace();
        let live = insert_source(&conn, ws, false);
        let off = insert_source(&conn, ws, true);
        let live_hit = insert_node(&conn, live, None, "notes.txt", "file");
        let off_dir = insert_node(&conn, off, None, "docs", "directory");
        insert_node(&conn, off, Some(off_dir), "notes.txt", "file");

        let res = search_nodes(&conn, ws, "notes", false).unwrap();
        assert_eq!(res.matched, vec![live_hit]);
        assert!(res.ancestors.is_empty());
    }
}
