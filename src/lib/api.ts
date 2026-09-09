// Typed wrappers over the Tauri command surface. Every backend command is
// funneled through here so components never touch `invoke` directly.

import { invoke } from '@tauri-apps/api/core';
import type {
    ConsolidationNode,
    GroupPage,
    GroupSort,
    HashScanReportDto,
    HashSettings,
    HashSpecDto,
    ImportSummary,
    MatchGroup,
    MediumInfoDto,
    NodeType,
    PathTreeNode,
    ScanPreview,
    ScanProgressInfo,
    SizeBucket,
    Source,
    TreeNode,
    Workspace,
} from './types';

// --- Workspaces ---

export const workspaceList = () => invoke<Workspace[]>('workspace_list');
export const workspaceCreate = (name: string) =>
    invoke<Workspace>('workspace_create', { name });
export const workspaceRename = (id: number, name: string) =>
    invoke<void>('workspace_rename', { id, name });
export const workspaceDelete = (id: number) =>
    invoke<void>('workspace_delete', { id });

// --- Sources ---

export const sourceList = (workspaceId: number) =>
    invoke<Source[]>('source_list', { workspaceId });
export const importTreeJson = (
    workspaceId: number,
    jsonText: string,
    label: string,
) => invoke<Source>('import_tree_json', { workspaceId, jsonText, label });
export const scanFolder = (
    workspaceId: number,
    path: string,
    label: string,
    hashingEnabled: boolean,
    hashMinSize?: number,
    mediumOverride?: string,
    filesystemOverride?: string,
) =>
    invoke<Source>('scan_folder', {
        workspaceId,
        path,
        label,
        hashMinSize: hashMinSize ?? null,
        mediumOverride: mediumOverride ?? null,
        filesystemOverride: filesystemOverride ?? null,
        hashingEnabled,
    });

// --- Medium detection & content hashing ---

export const detectMedium = (path: string) =>
    invoke<MediumInfoDto>('detect_medium', { path });
export const previewScan = (path: string, hashMinSize: number) =>
    invoke<ScanPreview>('preview_scan', { path, hashMinSize });
export const getSizeBuckets = (sourceId: number) =>
    invoke<SizeBucket[]>('get_size_buckets', { sourceId });
export const previewHashThreshold = (sourceId: number, hashMinSize: number) =>
    invoke<ScanPreview>('preview_hash_threshold', { sourceId, hashMinSize });
export const setHashMinSize = (sourceId: number, hashMinSize: number) =>
    invoke<void>('set_hash_min_size', { sourceId, hashMinSize });
export const hashSettingsGet = (workspaceId: number) =>
    invoke<HashSettings>('hash_settings_get', { workspaceId });
export const hashSettingsSet = (workspaceId: number, spec: HashSpecDto) =>
    invoke<void>('hash_settings_set', { workspaceId, spec });
export const runHashScan = (sourceId: number) =>
    invoke<HashScanReportDto>('run_hash_scan', { sourceId });
export const cancelHashScan = (sourceId: number) =>
    invoke<boolean>('cancel_hash_scan', { sourceId });
export const getScanProgress = (sourceId: number) =>
    invoke<ScanProgressInfo | null>('get_scan_progress', { sourceId });
export const sourceRenameDevice = (sourceId: number, deviceLabel: string) =>
    invoke<void>('source_rename_device', { sourceId, deviceLabel });
export const sourceSetExcluded = (sourceId: number, excluded: boolean) =>
    invoke<void>('source_set_excluded', { sourceId, excluded });
export const sourceCopyToWorkspace = (sourceId: number, targetWorkspaceId: number) =>
    invoke<void>('source_copy_to_workspace', { sourceId, targetWorkspaceId });
export const pathSeparator = () => invoke<string>('path_separator');

export const sourceDelete = (sourceId: number) =>
    invoke<void>('source_delete', { sourceId });

// --- Tree browsing ---

export const getTree = (
    workspaceId: number,
    sourceId: number,
    parentId: number | null,
) => invoke<TreeNode[]>('get_tree', { workspaceId, sourceId, parentId });

// --- Dedup ---

export const runDedup = (
    workspaceId: number,
    minSizeBytes: number,
    minConfidence: number,
) => invoke<number>('run_dedup', { workspaceId, minSizeBytes, minConfidence });
export const getGroups = (args: {
    workspaceId: number;
    minConfidence: number;
    minSize: number;
    kind?: 'file' | 'folder' | 'hardlink';
    sort?: GroupSort;
    offset: number;
    limit: number;
}) =>
    invoke<GroupPage>('get_groups', {
        workspaceId: args.workspaceId,
        minConfidence: args.minConfidence,
        minSize: args.minSize,
        kind: args.kind ?? null,
        sort: args.sort ?? 'confidence',
        offset: args.offset,
        limit: args.limit,
    });
export const getGroupForNode = (nodeId: number) =>
    invoke<MatchGroup | null>('get_group_for_node', { nodeId });

// --- Consolidation ---

export const consolidationGet = (workspaceId: number) =>
    invoke<[number, ConsolidationNode[]]>('consolidation_get', { workspaceId });
export const consolidationAddNode = (args: {
    consolidationId: number;
    parentId: number | null;
    name: string;
    nodeType: NodeType;
    sourceNodeId: number | null;
}) => invoke<ConsolidationNode>('consolidation_add_node', args);
export const consolidationAddSourceSubtree = (args: {
    consolidationId: number;
    parentId: number | null;
    sourceNodeId: number;
    /** Drop descendants hidden by this source's "exclusive to this device"
     *  funnel, so the materialized subtree matches what the user saw. */
    filterCrossDup: boolean;
}) => invoke<ConsolidationNode[]>('consolidation_add_source_subtree', args);
export const consolidationMoveNode = (
    nodeId: number,
    parentId: number | null,
    sortOrder: number,
) => invoke<void>('consolidation_move_node', { nodeId, parentId, sortOrder });
export const consolidationRenameNode = (nodeId: number, name: string) =>
    invoke<void>('consolidation_rename_node', { nodeId, name });
export const consolidationDeleteNode = (nodeId: number) =>
    invoke<void>('consolidation_delete_node', { nodeId });

// --- Path limits (on the consolidated end-state tree) ---

export const pathfixTree = (workspaceId: number, limit: number) =>
    invoke<PathTreeNode[]>('pathfix_tree', { workspaceId, limit });
export const pathfixRename = (
    workspaceId: number,
    nodeId: number,
    newName: string,
) => invoke<void>('pathfix_rename', { workspaceId, nodeId, newName });

// --- App state ---

export const appStateGet = (key: string) =>
    invoke<string | null>('app_state_get', { key });
export const appStateSet = (key: string, value: string) =>
    invoke<void>('app_state_set', { key, value });

export const workspaceStateGet = (workspaceId: number, key: string) =>
    invoke<string | null>('workspace_state_get', { workspaceId, key });
export const workspaceStateSet = (workspaceId: number, key: string, value: string) =>
    invoke<void>('workspace_state_set', { workspaceId, key, value });

// --- Database export / import ---

export const dbExport = (path: string) => invoke<void>('db_export', { path });
export const dbImport = (path: string) =>
    invoke<ImportSummary>('db_import', { path });
