// Shared types mirroring the Rust structs in `src-tauri/src/model.rs`.

export type NodeType = 'directory' | 'file' | 'link';
export type SourceKind = 'json' | 'scan';
export type ViewName = 'dedup' | 'consolidate' | 'pathlimits';

export interface Workspace {
    id: number;
    name: string;
    created_at: string;
    updated_at: string;
}

export interface Source {
    id: number;
    workspace_id: number;
    kind: SourceKind;
    label: string;
    device_label: string;
    orig_root_path: string | null;
    dev_id: number | null;
    imported_at: string;
    total_size: number;
    file_count: number;
    excluded: boolean;
    duplicated_pct: number;
    cross_dup_size: number;
    cross_dup_file_count: number;
    /** `total_size` minus bytes double-counted by hardlink alias sets --
     *  what the UI should display as the device's real disk usage. */
    physical_size: number;
    /** Bytes among `total_size` that belong to non-canonical hardlink aliases. */
    alias_bytes: number;
    /** Detected (or user-overridden) storage medium: "hdd" | "ssd" | "network"
     *  | "optical" | "unknown". `null` for JSON imports and pre-hashing sources. */
    medium_kind: string | null;
    /** Detected (or user-overridden) filesystem name ("NTFS", "exFAT", "ext4", ...). */
    filesystem: string | null;
    /** Files smaller than this were never queued for content hashing. */
    hash_min_size: number;
    /** The workspace hash spec in force when this source was hashed. */
    hash_spec: string | null;
    hash_coverage_files: number;
    hash_coverage_bytes: number;
    /** Whether content hashing runs for this source at all. When false, the
     *  device is matched on filesystem metadata only. */
    hashing_enabled: boolean;
}

export type MediumKind = 'hdd' | 'ssd' | 'network' | 'optical' | 'unknown';

export interface MediumInfoDto {
    medium_kind: MediumKind;
    filesystem: string | null;
    volume_id: string;
}

/** A cheap walk-only dry run (no DB writes) for the scan-config dialog. */
export interface ScanPreview {
    total_files: number;
    total_bytes: number;
    files_above_threshold: number;
    bytes_above_threshold: number;
}

/** `threshold: null` means "full hash everything" -- no sampling. */
export interface HashSpecDto {
    threshold: number | null;
    probe: number;
    stride: number;
    spec_string: string;
}

/** Returned by `hash_settings_get`. Once `locked`, the spec fields in the
 *  scan-config dialog must be shown read-only, not editable. */
export interface HashSettings {
    spec: HashSpecDto;
    locked: boolean;
}

export interface HashScanReportDto {
    hashed: number;
    cached: number;
    errors: number;
}

export interface HashProgress {
    current: number;
    total: number;
}

/** `phase === "hashing"` means a previous `run_hash_scan` was interrupted
 *  and should show a "Resume hashing" affordance. */
export interface ScanProgressInfo {
    phase: string;
    last_cursor: number;
}

export interface TreeNode {
    id: number;
    source_id: number;
    parent_id: number | null;
    name: string;
    rel_path: string;
    type: NodeType;
    size: number;
    mtime: string | null;
    inode: number | null;
    dev: number | null;
    depth: number;
    subtree_size: number;
    subtree_file_count: number;
    has_duplicate: boolean;
    dup_pct: number;
    cross_dup: boolean;
    cross_dup_size: number;
    cross_dup_file_count: number;
    in_folder_group: boolean;
    /** Canonical node id when this is a hardlink alias. */
    alias_of: number | null;
    /** Whether this node is part of a hardlink alias set at all -- either
     *  an alias or the canonical target of one or more aliases. Takes
     *  precedence over `has_duplicate` in the tree UI's badge. */
    is_hardlink: boolean;
}

export interface MatchMember {
    node_id: number;
    source_id: number;
    device_label: string;
    rel_path: string;
    name: string;
    size: number;
    mtime: string | null;
}

export interface MatchGroup {
    id: number;
    workspace_id: number;
    kind: 'file' | 'folder' | 'hardlink';
    confidence: number;
    primary_signal: string;
    size: number;
    members: MatchMember[];
}

export interface GroupPage {
    total: number;
    groups: MatchGroup[];
}

export type GroupSort = 'confidence' | 'size';

export interface DeviceStats {
    source_id: number;
    device_label: string;
    total_size: number;
    file_count: number;
    duplicated_size: number;
    duplicated_pct: number;
}

export interface ConsolidationNode {
    id: number;
    consolidation_id: number;
    parent_id: number | null;
    name: string;
    type: NodeType;
    source_node_id: number | null;
    sort_order: number;
    size: number | null;
    origin_device: string | null;
    origin_path: string | null;
}

export interface PathTreeNode {
    id: number;
    parent_id: number | null;
    name: string;
    original_name: string;
    type: NodeType;
    edited: boolean;
    over_limit: boolean;
    path_length: number;
    sort_order: number;
}

export interface DedupProgress {
    phase: string;
    current: number;
    total: number;
}

export interface ScanProgress {
    current: number;
}

/** What `ScanConfigDialog` hands back on confirm -- everything needed to
 *  run scan -> hash -> dedup for one newly-picked folder. */
export interface ScanConfig {
    hashSpec: HashSpecDto;
    specLocked: boolean;
    hashMinSize: number;
    mediumOverride: string;
    filesystemOverride: string;
    hashingEnabled: boolean;
}

export interface ImportSummary {
    workspaces_added: number;
    sources_added: number;
    nodes_added: number;
    new_workspace_ids: number[];
}
