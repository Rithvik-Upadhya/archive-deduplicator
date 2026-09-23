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
    /** Every *name* on this device: canonical files, hardlink aliases and
     *  symlinks alike, so it matches what manual inspection of the source
     *  would show. */
    file_count: number;
    excluded: boolean;
    /** Percentage (0-100) of this device's *physical* bytes duplicated
     *  elsewhere. The base is `physical_size`, not `total_size`: hardlink
     *  aliases never enter the matcher, so they can never be in the numerator. */
    duplicated_pct: number;
    /** The cross-source share of `duplicated_pct`, over the same
     *  `physical_size` base; the remainder is duplication internal to this
     *  device. Split from one pass in `duplicated_size_by_source`, so the two
     *  always sum back to `duplicated_pct` -- which is what lets the source
     *  bar draw them as two segments of one whole. Not derived from
     *  `cross_dup_size`: that answers what the funnel hides, over a different
     *  population. */
    cross_duplicated_pct: number;
    /** Canonical-file bytes the funnel hides. Subtract from `physical_size`. */
    cross_dup_size: number;
    /** *Names* the funnel hides -- canonical files, hardlink aliases and
     *  symlinks alike. Subtract from `file_count`, which counts names too. */
    cross_dup_file_count: number;
    /** Hidden bytes belonging to hardlink aliases. Not part of `cross_dup_size`
     *  (alias bytes were never in `physical_size`) -- it lets the header's
     *  "N hardlinked" annotation shrink to what is still on screen. */
    cross_dup_alias_bytes: number;
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
    /** `"hashing"` means the last hash pass was paused or interrupted before
     *  finishing (a "Resume" affordance applies), `"done"` means it finished,
     *  `null` means hashing was never started. */
    hashing_phase: string | null;
    /** User-picked `#rrggbb` for attributing consolidated nodes to this
     *  source; `null` means never picked -- resolve through `sourceColor`. */
    color: string | null;
}

export type MediumKind = 'hdd' | 'ssd' | 'network' | 'optical' | 'unknown';

export interface MediumInfoDto {
    medium_kind: MediumKind;
    filesystem: string | null;
    volume_id: string;
}

/** A cheap walk-only dry run (no DB writes) for the scan-config dialog. Also
 *  reused post-scan by `previewHashThreshold`, which computes the same shape
 *  from already-scanned nodes instead of a disk walk. */
export interface ScanPreview {
    total_files: number;
    total_bytes: number;
    files_above_threshold: number;
    bytes_above_threshold: number;
}

/** One row of the post-scan file-size distribution table, shown in the
 *  hash-threshold refinement step right after a scan completes. */
export interface SizeBucket {
    bucket: string;
    files: number;
    gib: number;
    pct_files: number;
    pct_bytes: number;
}

/** `threshold: null` means "full hash everything" -- no sampling. */
export interface HashSpecDto {
    threshold: number | null;
    probe: number;
    max_probes: number;
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
    /** True if the pass stopped early because it was cancelled, rather than
     *  finishing every candidate. */
    cancelled: boolean;
}

export interface HashProgress {
    source_id: number;
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
    /** A safety cap in the matcher declined to judge this node. Distinct from
     *  "no duplicate found" -- no verdict was reached, so the tree must not
     *  present it as exclusive to this device. */
    skipped: boolean;
    /** Directories only: how many nodes in this subtree are `skipped`. */
    skipped_count: number;
}

export interface MatchMember {
    node_id: number;
    source_id: number;
    device_label: string;
    rel_path: string;
    name: string;
    size: number;
    subtree_size: number;
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
    /** `sources.id` of the origin device -- the key for per-source colouring
     *  (`origin_device` is a renamable label and need not be unique). */
    origin_source_id: number | null;
    /** Whether the origin node is a hardlink alias. Still counts as a file --
     *  it is a name someone has to recreate -- but its bytes belong to the
     *  canonical, so size rollups must skip them. */
    is_alias: boolean;
    /** Archivist progress mark: this row has been dealt with on the real
     *  filesystem. Applies to this row only. */
    done: boolean;
    /** "To delete" mark -- the row's *own* flag. A row is effectively struck
     *  when it or any ancestor carries it; struck subtrees are left out of the
     *  consolidated totals and of Fix Paths. */
    struck: boolean;
    /** Name before the user renamed this row; null when it isn't renamed. */
    original_name: string | null;
}

/** Outcome of `pathfix_rename`: the name afterwards, and the original while
 *  the row is still renamed (null once reverted). */
export interface RenameResult {
    name: string;
    original_name: string | null;
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
    /** Device this node was dragged in from, and its path on that device. Both
     *  null for a folder the user created by hand. Shown on hover -- and
     *  deliberately not what the copy button yields, which is the source-free
     *  end-state path. */
    origin_device: string | null;
    origin_path: string | null;
    /** `sources.id` of the origin device, for the per-source colour bars. */
    origin_source_id: number | null;
    /** Struck by its own flag or an ancestor's. Shown, but never measured:
     *  a struck row is never `over_limit` and never makes an ancestor so. */
    struck: boolean;
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
