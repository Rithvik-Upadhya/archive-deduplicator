//! Shared data types serialized between the Rust backend and the SvelteKit
//! frontend. These mirror the TypeScript definitions in `src/lib/types.ts`.

use serde::{Deserialize, Serialize};

/// A single node as produced by the `tree -JDs --inodes --device` command.
/// Used only while parsing an uploaded JSON file.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeNode {
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub inode: Option<i64>,
    #[serde(default)]
    pub dev: Option<i64>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub contents: Option<Vec<TreeNode>>,
}

/// A workspace groups a set of sources (devices/discs) that are deduplicated together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A source represents one imported device/disc (a JSON tree or a folder scan).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub workspace_id: i64,
    pub kind: String,
    pub label: String,
    /// User-renamable device label shown in the UI.
    pub device_label: String,
    pub orig_root_path: Option<String>,
    pub dev_id: Option<i64>,
    pub imported_at: String,
    pub total_size: i64,
    pub file_count: i64,
    /// Percentage (0-100) of this device's bytes that appear duplicated elsewhere.
    #[serde(default)]
    pub duplicated_pct: f64,
}

/// A flattened filesystem node stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: i64,
    pub source_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub rel_path: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub size: i64,
    pub mtime: Option<String>,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub depth: i64,
    pub subtree_size: i64,
    pub subtree_file_count: i64,
    /// Whether this node participates in any cross-source match group.
    #[serde(default)]
    pub has_duplicate: bool,
    /// For directories: fraction (0-100) of subtree bytes that appear duplicated.
    #[serde(default)]
    pub dup_pct: f64,
}

/// A group of nodes that are likely duplicates of one another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchGroup {
    pub id: i64,
    pub workspace_id: i64,
    pub kind: String,
    pub confidence: f64,
    pub primary_signal: String,
    pub size: i64,
    #[serde(default)]
    pub members: Vec<MatchMember>,
}

/// One page of match groups plus the total count matching the filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupPage {
    pub total: i64,
    pub groups: Vec<MatchGroup>,
}

/// A member node of a match group, annotated with display context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchMember {
    pub node_id: i64,
    pub source_id: i64,
    pub device_label: String,
    pub rel_path: String,
    pub name: String,
    pub size: i64,
    pub mtime: Option<String>,
}

/// A node in a consolidation (target) tree the user is assembling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationNode {
    pub id: i64,
    pub consolidation_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub source_node_id: Option<i64>,
    pub action: String,
    pub sort_order: i64,
}

/// An entry in the change/guide log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionLogEntry {
    pub id: i64,
    pub workspace_id: i64,
    pub ts: String,
    pub op: String,
    pub detail: String,
}

/// Per-source (device) statistics summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStats {
    pub source_id: i64,
    pub device_label: String,
    pub total_size: i64,
    pub file_count: i64,
    pub duplicated_size: i64,
    pub duplicated_pct: f64,
}

/// One editable component (folder or file) of an over-limit path in the
/// consolidated (end-state) tree. `kind` says which table the id refers to:
/// "cons" -> consolidation_nodes, "source" -> nodes (virtual rename edit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathComponent {
    pub node_id: i64,
    pub kind: String,
    /// Effective name (rename edit applied when present).
    pub name: String,
    /// Original name before any edit.
    pub original_name: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub edited: bool,
}

/// A branch of the consolidated end-state tree whose full path exceeds a
/// length limit (default 260 for Windows).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathLimitEntry {
    /// Leaf id (in the table indicated by `leaf_kind`).
    pub node_id: i64,
    pub leaf_kind: String,
    pub effective_path: String,
    pub length: i64,
    pub resolved: bool,
    /// Every component of the path from root to leaf, each editable.
    pub components: Vec<PathComponent>,
}

/// Progress payload emitted while a long-running dedup pass is executing.
#[derive(Debug, Clone, Serialize)]
pub struct DedupProgress {
    pub phase: String,
    pub current: u64,
    pub total: u64,
}
