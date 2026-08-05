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
    /// When true, this source is left out of the matcher (and its tree panels
    /// are hidden) until re-included, without deleting any of its data.
    #[serde(default)]
    pub excluded: bool,
    /// Percentage (0-100) of this device's bytes that appear duplicated elsewhere.
    #[serde(default)]
    pub duplicated_pct: f64,
    /// Bytes among `total_size` that are cross-device duplicates -- what the
    /// "exclusive to this device" filter hides. Subtract from `total_size` /
    /// `file_count` to get the filtered totals.
    #[serde(default)]
    pub cross_dup_size: i64,
    #[serde(default)]
    pub cross_dup_file_count: i64,
    /// `total_size` minus bytes double-counted by hardlink alias sets. This
    /// is what the UI should display as the device's real disk usage.
    #[serde(default)]
    pub physical_size: i64,
    /// Bytes among `total_size` that belong to non-canonical hardlink
    /// aliases (`(k-1) * size` per alias set).
    #[serde(default)]
    pub alias_bytes: i64,
    /// Detected (or user-overridden) storage medium at scan time: "hdd",
    /// "ssd", "network", "optical", "unknown". `None` for JSON imports and
    /// any source scanned before this field existed.
    #[serde(default)]
    pub medium_kind: Option<String>,
    /// Detected (or user-overridden) filesystem name ("NTFS", "exFAT", "ext4", ...).
    /// Gates hardlink-collapse trust -- see `medium::filesystem_is_trusted`.
    #[serde(default)]
    pub filesystem: Option<String>,
    /// Files smaller than this were never queued for content hashing.
    #[serde(default)]
    pub hash_min_size: i64,
    /// The workspace hash spec in force when this source was hashed, copied
    /// here so cross-source hash comparability is verifiable from the data.
    #[serde(default)]
    pub hash_spec: Option<String>,
    #[serde(default)]
    pub hash_coverage_files: i64,
    #[serde(default)]
    pub hash_coverage_bytes: i64,
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
    /// Whether the "exclusive to this device" filter should hide this node:
    /// for a file, a duplicate exists on a different device (source_id); for
    /// a directory, every leaf in its subtree is such a file.
    #[serde(default)]
    pub cross_dup: bool,
    /// Directories only: byte size / file count within `subtree_size` /
    /// `subtree_file_count` that `cross_dup` would hide, so the UI can show
    /// filtered totals without re-walking the tree client-side.
    #[serde(default)]
    pub cross_dup_size: i64,
    #[serde(default)]
    pub cross_dup_file_count: i64,
    /// Directories only: whether this directory is a genuine member of a
    /// clustered folder-kind match group (as opposed to merely having
    /// `dup_pct > 0`) -- gates whether "Locate duplicates" has anything to find.
    #[serde(default)]
    pub in_folder_group: bool,
    /// Canonical node id when this is a hardlink alias (a non-canonical
    /// name for a physical file that also exists under another name).
    #[serde(default)]
    pub alias_of: Option<i64>,
    /// Whether this node is part of a hardlink alias set at all -- either
    /// an alias (`alias_of` set) or the canonical target of one or more
    /// aliases. Drives the "link" badge in the tree UI, which takes
    /// precedence over the normal "dup" badge for these nodes.
    #[serde(default)]
    pub is_hardlink: bool,
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
    pub sort_order: i64,
    /// File size in bytes; only meaningful when `node_type == "file"`.
    #[serde(default)]
    pub size: Option<i64>,
    /// `sources.device_label` of the origin device, when dragged from a source.
    #[serde(default)]
    pub origin_device: Option<String>,
    /// `nodes.rel_path` (full path from the source root) of the origin node.
    #[serde(default)]
    pub origin_path: Option<String>,
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

/// One node in the consolidated end-state tree, annotated with path-length
/// guidance. Produced by `pathfix::build_tree`. The frontend renders a real
/// nested tree from this flat-with-parent-links list (same idiom as
/// `ConsolidationNode`), instead of the old flat breadcrumb list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathTreeNode {
    /// consolidation_nodes.id.
    pub id: i64,
    /// Parent's id. `None` only for a tree root.
    pub parent_id: Option<i64>,
    /// Effective name (rename edit applied when present).
    pub name: String,
    /// Name before any edit.
    pub original_name: String,
    #[serde(rename = "type")]
    pub node_type: String,
    pub edited: bool,
    /// True if this node lies on the root-to-leaf chain of ANY leaf whose
    /// full effective path exceeds the configured limit. Propagated to every
    /// ancestor of such a leaf, not just the node where cumulative length
    /// first crosses the threshold.
    pub over_limit: bool,
    /// Character length (Unicode-scalar count) of this node's own full
    /// effective path from the tree root. Computed for every node; only
    /// meaningful to *display* on leaves.
    pub path_length: i64,
    /// consolidation_nodes.sort_order, passed through so the frontend can
    /// compute a correct append position when moving a node via drag-drop.
    pub sort_order: i64,
}

/// Progress payload emitted while a long-running dedup pass is executing.
#[derive(Debug, Clone, Serialize)]
pub struct DedupProgress {
    pub phase: String,
    pub current: u64,
    pub total: u64,
}

/// Progress payload emitted while a folder scan is executing. There is no
/// `total` -- see the doc comment on `scan::scan_folder` for why.
#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub current: u64,
}

/// Detected storage-medium/filesystem info for a scan root, shown in the
/// scan-configuration dialog before the user commits to scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediumInfoDto {
    pub medium_kind: String,
    pub filesystem: Option<String>,
    pub volume_id: String,
}

/// A cheap walk-only dry run (no DB writes) used to show real numbers --
/// "~87,300 of 337,000 files will be hashed" -- in the scan-config dialog.
#[derive(Debug, Clone, Serialize)]
pub struct ScanPreview {
    pub total_files: i64,
    pub total_bytes: i64,
    pub files_above_threshold: i64,
    pub bytes_above_threshold: i64,
}

/// A workspace's digest-affecting hash settings (§6.1). `spec_string` is the
/// canonical encoding stored on `sources.hash_spec`/`nodes.hash_spec`;
/// `threshold: None` means "full hash everything".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashSpecDto {
    pub threshold: Option<i64>,
    pub probe: i64,
    pub stride: i64,
    pub spec_string: String,
}

/// Returned by `hash_settings_get`. Once `locked`, the workspace settings UI
/// should show these read-only rather than editable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashSettings {
    pub spec: HashSpecDto,
    pub locked: bool,
}

/// Summary of one `run_hash_scan` call (which may itself be a resume).
#[derive(Debug, Clone, Serialize)]
pub struct HashScanReportDto {
    pub hashed: i64,
    pub cached: i64,
    pub errors: i64,
}

/// Progress payload emitted while a hashing pass is executing.
#[derive(Debug, Clone, Serialize)]
pub struct HashProgress {
    pub current: u64,
    pub total: u64,
}

/// Current `scan_progress` state for a source -- drives a "Resume hashing"
/// banner when `phase == "hashing"` (interrupted) rather than absent (never
/// started) or `"done"`.
#[derive(Debug, Clone, Serialize)]
pub struct ScanProgressInfo {
    pub phase: String,
    pub last_cursor: i64,
}
