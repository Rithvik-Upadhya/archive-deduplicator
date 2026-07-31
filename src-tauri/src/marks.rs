//! Keeper decisions: which copy of a duplicated file or folder is definitive.
//!
//! This app never touches the filesystem — it produces a plan. The dedup view's
//! output is therefore a set of *decisions*, which the consolidation step reads
//! to hide the copies the user has already ruled out.
//!
//! Marks are stored per node rather than per match group because `dedup::run`
//! deletes and rebuilds every group on each pass, so group ids do not survive a
//! re-run. Node ids do.
//!
//! A mark on a directory applies to its whole subtree, which is what makes the
//! workflow cheap: marking one `keep` folder can decide thousands of groups.
//! Inheritance is resolved in SQL rather than by materializing the covered
//! nodes — marking a folder like `GitHub` would otherwise mean expanding 200k+
//! node ids on every page query.

use rusqlite::{params, Connection};

/// A user decision about one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Keep,
    Drop,
}

impl Mark {
    pub fn parse(s: &str) -> Option<Mark> {
        match s {
            "keep" => Some(Mark::Keep),
            "drop" => Some(Mark::Drop),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mark::Keep => "keep",
            Mark::Drop => "drop",
        }
    }
}

/// The state of a match group given the current marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// No member is marked as the copy to keep.
    Undecided,
    /// Exactly one member is the keeper.
    Decided(i64),
    /// More than one member is marked `keep` — the user must disambiguate.
    Conflict,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Undecided => "undecided",
            Decision::Decided(_) => "decided",
            Decision::Conflict => "conflict",
        }
    }

    pub fn keeper(self) -> Option<i64> {
        match self {
            Decision::Decided(id) => Some(id),
            _ => None,
        }
    }
}

/// An explicit mark row, joined to the node it was set on.
pub struct StoredMark {
    pub source_id: i64,
    pub rel_path: String,
    pub node_type: String,
    pub mark: Mark,
}

/// Load every explicit mark in the workspace. One row per user decision, so
/// this stays small regardless of how large the scanned trees are.
pub fn load_stored(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Vec<StoredMark>> {
    let mut stmt = conn.prepare(
        "SELECT nm.node_id, n.source_id, n.rel_path, n.type, nm.mark
         FROM node_marks nm
         JOIN nodes n ON n.id = nm.node_id
         WHERE nm.workspace_id = ?1",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (source_id, rel_path, node_type, mark) = row?;
        if let Some(mark) = Mark::parse(&mark) {
            out.push(StoredMark {
                source_id,
                rel_path,
                node_type,
                mark,
            });
        }
    }
    Ok(out)
}

/// Resolves effective marks in memory.
///
/// Expressing inheritance as a correlated SQL subquery is not viable: SQLite
/// drives such a subquery from `nodes` and re-scans a marked subtree for every
/// candidate row, which on a 250k-node folder never finishes. The marks
/// themselves are few (one per user decision) and only ever need to be applied
/// to *group members* (tens of thousands), so resolving them here is both
/// simpler and orders of magnitude faster.
pub struct Resolver {
    stored: Vec<StoredMark>,
}

impl Resolver {
    pub fn load(conn: &Connection, workspace_id: i64) -> rusqlite::Result<Resolver> {
        Ok(Resolver {
            stored: load_stored(conn, workspace_id)?,
        })
    }

    /// The mark that applies to a node: its own if set, otherwise the one from
    /// the nearest marked ancestor. A longer ancestor path is the more specific
    /// instruction, and an exact match (the node itself) is longest of all, so
    /// explicit always beats inherited.
    pub fn effective(&self, source_id: i64, rel_path: &str) -> Option<(Mark, bool)> {
        let mut best: Option<&StoredMark> = None;
        for sm in &self.stored {
            if sm.source_id != source_id {
                continue;
            }
            let covers = sm.rel_path == rel_path
                || (sm.node_type == "directory"
                    && rel_path
                        .strip_prefix(&sm.rel_path)
                        .is_some_and(|rest| rest.starts_with('/')));
            if !covers {
                continue;
            }
            if best.is_none_or(|b| sm.rel_path.len() > b.rel_path.len()) {
                best = Some(sm);
            }
        }
        best.map(|b| (b.mark, b.rel_path == rel_path))
    }

    /// Materialize into `temp.eff_keep` every match-group member whose
    /// effective mark is `keep`, so group decisions become an indexed join
    /// instead of a per-row subquery. Returns false when nothing is marked.
    pub fn materialize_keep_set(
        &self,
        conn: &Connection,
        workspace_id: i64,
    ) -> rusqlite::Result<bool> {
        conn.execute_batch(
            "DROP TABLE IF EXISTS temp.eff_keep;
             CREATE TEMP TABLE eff_keep (node_id INTEGER PRIMARY KEY);",
        )?;
        if self.stored.is_empty() {
            return Ok(false);
        }

        // Only group members can affect a decision, so the scan is bounded by
        // the number of members rather than the size of the marked subtrees.
        // Scoping through `sources` rather than `match_groups` lets SQLite use
        // the node index directly — 17ms instead of 265ms on a 50k-member set —
        // and the INSERT's primary key absorbs the duplicates a DISTINCT would.
        let members: Vec<(i64, i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT mm.node_id, n.source_id, n.rel_path
                 FROM match_members mm
                 JOIN nodes n ON n.id = mm.node_id
                 JOIN sources s ON s.id = n.source_id
                 WHERE s.workspace_id = ?1",
            )?;
            stmt.query_map(params![workspace_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut stmt = conn.prepare("INSERT OR IGNORE INTO temp.eff_keep (node_id) VALUES (?1)")?;
        for (node_id, source_id, rel_path) in &members {
            if let Some((Mark::Keep, _)) = self.effective(*source_id, rel_path) {
                stmt.execute(params![node_id])?;
            }
        }
        Ok(true)
    }
}

/// SQL scalar subquery counting the members of the group bound to `alias` whose
/// effective mark is `keep`: 0 = undecided, 1 = decided, >1 = conflict.
/// Requires `temp.eff_keep` to have been materialized first.
pub fn keep_count_sql(alias: &str) -> String {
    format!(
        "(SELECT COUNT(*) FROM match_members mk
            JOIN temp.eff_keep k ON k.node_id = mk.node_id
           WHERE mk.group_id = {alias}.id)"
    )
}

/// Resolve a group's decision from its members' already-loaded effective marks.
pub fn decide(members: &[crate::model::MatchMember]) -> Decision {
    let mut keepers = members.iter().filter(|m| m.mark.as_deref() == Some("keep"));
    match (keepers.next(), keepers.next()) {
        (Some(only), None) => Decision::Decided(only.node_id),
        (Some(_), Some(_)) => Decision::Conflict,
        _ => Decision::Undecided,
    }
}

/// Set or clear the explicit mark on a node. `None` clears it.
pub fn set_mark(
    conn: &Connection,
    workspace_id: i64,
    node_id: i64,
    mark: Option<Mark>,
    now: &str,
) -> rusqlite::Result<()> {
    match mark {
        Some(mark) => {
            conn.execute(
                "INSERT INTO node_marks (node_id, workspace_id, mark, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node_id) DO UPDATE SET mark = excluded.mark,
                                                    created_at = excluded.created_at",
                params![node_id, workspace_id, mark.as_str(), now],
            )?;
        }
        None => {
            conn.execute("DELETE FROM node_marks WHERE node_id = ?1", params![node_id])?;
        }
    }
    Ok(())
}

/// Clear every explicit mark on a node and everything beneath it, so a user can
/// start a subtree's decisions over.
pub fn clear_subtree(conn: &Connection, workspace_id: i64, node_id: i64) -> rusqlite::Result<usize> {
    let (source_id, rel_path): (i64, String) = conn.query_row(
        "SELECT source_id, rel_path FROM nodes WHERE id = ?1",
        params![node_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    conn.execute(
        "DELETE FROM node_marks
         WHERE workspace_id = ?1
           AND node_id IN (SELECT id FROM nodes
                           WHERE source_id = ?2
                             AND (rel_path = ?3 OR rel_path LIKE ?3 || '/%'))",
        params![workspace_id, source_id, rel_path],
    )
}
