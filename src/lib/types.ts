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
    kind: 'file' | 'folder';
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

export interface ImportSummary {
    workspaces_added: number;
    sources_added: number;
    nodes_added: number;
    new_workspace_ids: number[];
}
