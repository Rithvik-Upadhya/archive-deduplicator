// Typed wrappers over the Tauri command surface. Every backend command is
// funneled through here so components never touch `invoke` directly.

import { invoke } from '@tauri-apps/api/core';
import type {
    ActionLogEntry,
    ConsolidationAction,
    ConsolidationNode,
    DeviceStats,
    GroupPage,
    GroupSort,
    ImportSummary,
    MatchGroup,
    NodeType,
    PathComponentKind,
    PathLimitEntry,
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
export const scanFolder = (workspaceId: number, path: string, label: string) =>
    invoke<Source>('scan_folder', { workspaceId, path, label });
export const sourceRenameDevice = (sourceId: number, deviceLabel: string) =>
    invoke<void>('source_rename_device', { sourceId, deviceLabel });
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
    kind?: 'file' | 'folder';
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
export const getDeviceStats = (workspaceId: number) =>
    invoke<DeviceStats[]>('get_device_stats', { workspaceId });

// --- Consolidation ---

export const consolidationGet = (workspaceId: number) =>
    invoke<[number, ConsolidationNode[]]>('consolidation_get', { workspaceId });
export const consolidationAddNode = (args: {
    workspaceId: number;
    consolidationId: number;
    parentId: number | null;
    name: string;
    nodeType: NodeType;
    sourceNodeId: number | null;
    action: ConsolidationAction;
}) => invoke<ConsolidationNode>('consolidation_add_node', args);
export const consolidationSetAction = (
    workspaceId: number,
    nodeId: number,
    action: ConsolidationAction,
) => invoke<void>('consolidation_set_action', { workspaceId, nodeId, action });
export const consolidationMoveNode = (
    nodeId: number,
    parentId: number | null,
    sortOrder: number,
) => invoke<void>('consolidation_move_node', { nodeId, parentId, sortOrder });
export const consolidationRenameNode = (
    workspaceId: number,
    nodeId: number,
    name: string,
) => invoke<void>('consolidation_rename_node', { workspaceId, nodeId, name });
export const consolidationDeleteNode = (nodeId: number) =>
    invoke<void>('consolidation_delete_node', { nodeId });

// --- Action log ---

export const actionLogList = (workspaceId: number, opPrefix?: string) =>
    invoke<ActionLogEntry[]>('action_log_list', {
        workspaceId,
        opPrefix: opPrefix ?? null,
    });
export const exportActionLog = (workspaceId: number) =>
    invoke<string>('export_action_log', { workspaceId });

// --- Path limits (on the consolidated end-state tree) ---

export const pathfixList = (workspaceId: number, limit: number) =>
    invoke<PathLimitEntry[]>('pathfix_list', { workspaceId, limit });
export const pathfixRename = (
    workspaceId: number,
    kind: PathComponentKind,
    nodeId: number,
    newName: string,
) => invoke<void>('pathfix_rename', { workspaceId, kind, nodeId, newName });
export const pathfixSetResolved = (
    workspaceId: number,
    kind: PathComponentKind,
    nodeId: number,
    resolved: boolean,
) =>
    invoke<void>('pathfix_set_resolved', {
        workspaceId,
        kind,
        nodeId,
        resolved,
    });

// --- App state ---

export const appStateGet = (key: string) =>
    invoke<string | null>('app_state_get', { key });
export const appStateSet = (key: string, value: string) =>
    invoke<void>('app_state_set', { key, value });

// --- File export ---

export const writeTextFile = (path: string, contents: string) =>
    invoke<void>('write_text_file', { path, contents });

// --- Database export / import ---

export const dbExport = (path: string) => invoke<void>('db_export', { path });
export const dbImport = (path: string) =>
    invoke<ImportSummary>('db_import', { path });
