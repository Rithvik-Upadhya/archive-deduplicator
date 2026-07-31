// Central application state using Svelte 5 runes. A single shared instance is
// exported; components import it and read/mutate its reactive fields. All
// mutations that matter to persistence are pushed to the Rust/SQLite backend.

import * as api from '../api';
import type {
    DecisionFilter,
    DeviceStats,
    FolderReport,
    GroupSort,
    ImportSummary,
    MatchGroup,
    NodeMark,
    NodeMarkValue,
    Source,
    TreeNode,
    ViewName,
    Workspace,
} from '../types';

const GROUP_PAGE_SIZE = 50;

/**
 * What the review panel can be focused on. Kept narrower than `TreeNode` so a
 * folder reached from the overlap list — which was never expanded in the tree,
 * and so has no `TreeNode` — can scope the panel just as well as a tree row.
 */
export type ScopeTarget = Pick<
    TreeNode,
    'id' | 'source_id' | 'name' | 'rel_path' | 'type'
>;

/** Navigation metadata shared by the sidebar and the breadcrumb. */
export const VIEWS: { id: ViewName; label: string; icon: string }[] = [
    { id: 'dedup', label: 'Deduplicate', icon: 'ph:copy-simple-fill' },
    { id: 'consolidate', label: 'Consolidate', icon: 'ph:tree-view-fill' },
    { id: 'pathlimits', label: 'Fix Paths', icon: 'ph:password-fill' },
];

export function viewLabel(view: ViewName): string {
    return VIEWS.find((v) => v.id === view)?.label ?? view;
}

class AppState {
    workspaces = $state<Workspace[]>([]);
    activeWorkspaceId = $state<number | null>(null);
    sources = $state<Source[]>([]);
    view = $state<ViewName>('dedup');

    // Dedup tuning parameters (persisted in app_state).
    minSizeKb = $state(4);
    minConfidence = $state(40);

    // Results (paged: `groups` holds all pages loaded so far).
    groups = $state<MatchGroup[]>([]);
    groupTotal = $state(0);
    groupSort = $state<GroupSort>('confidence');
    groupKind = $state<'all' | 'file' | 'folder'>('all');
    groupsLoading = $state(false);
    deviceStats = $state<DeviceStats[]>([]);

    /**
     * The subtree the review panel is focused on. A folder showing "55% dup" is
     * rarely a duplicate *itself* — the redundancy is in the files inside it —
     * so selecting a node scopes the group list to its subtree rather than
     * looking up a group the node belongs to.
     */
    scopeNode = $state<ScopeTarget | null>(null);
    folderReport = $state<FolderReport | null>(null);
    reportLoading = $state(false);
    decisionFilter = $state<DecisionFilter>('all');

    /** Explicit keeper decisions, keyed by node id. Inheritance is applied when
     *  rendering — see `effectiveMark`. */
    marks = $state<Map<number, NodeMark>>(new Map());

    /** True when sources changed after the last dedup run (nudges a re-run). */
    dedupStale = $state(false);

    running = $state(false);
    loading = $state(true);

    /** Guards against `init()` running more than once. */
    private started = false;

    get activeWorkspace(): Workspace | null {
        return this.workspaces.find((w) => w.id === this.activeWorkspaceId) ?? null;
    }

    async init() {
        if (this.started) return;
        this.started = true;
        this.loading = true;
        try {
            this.workspaces = await api.workspaceList();
            if (this.workspaces.length === 0) {
                const ws = await api.workspaceCreate('Workspace 1');
                this.workspaces = [ws];
            }
            const lastId = await api.appStateGet('active_workspace');
            const parsed = lastId ? Number(lastId) : null;
            this.activeWorkspaceId =
                parsed && this.workspaces.some((w) => w.id === parsed)
                    ? parsed
                    : this.workspaces[0].id;

            const view = (await api.appStateGet('view')) as ViewName | null;
            if (view) this.view = view;
            const minSize = await api.appStateGet('min_size_kb');
            if (minSize) this.minSizeKb = Number(minSize);
            const minConf = await api.appStateGet('min_confidence');
            if (minConf) this.minConfidence = Number(minConf);
            const stale = await api.appStateGet('dedup_stale');
            this.dedupStale = stale === '1';
            const decision = (await api.appStateGet(
                'decision_filter',
            )) as DecisionFilter | null;
            if (decision) this.decisionFilter = decision;

            await this.loadWorkspace();
        } finally {
            this.loading = false;
        }
    }

    async loadWorkspace() {
        if (this.activeWorkspaceId == null) return;
        const ws = this.activeWorkspaceId;
        this.sources = await api.sourceList(ws);
        this.deviceStats = await api.getDeviceStats(ws);
        // Keep the scope across a dedup re-run — that is exactly when a user
        // wants to see the same folder's results again — but drop it if the
        // source it points into is gone.
        if (
            this.scopeNode &&
            !this.sources.some((s) => s.id === this.scopeNode!.source_id)
        ) {
            this.scopeNode = null;
            this.folderReport = null;
        }
        await this.loadMarks();
        await this.refreshGroups();
        await this.refreshReport();
    }

    async loadMarks() {
        if (this.activeWorkspaceId == null) return;
        const rows = await api.getMarks(this.activeWorkspaceId);
        this.marks = new Map(rows.map((m) => [m.node_id, m]));
    }

    /**
     * The mark that applies to a node: its own if set, otherwise the one from
     * the nearest marked ancestor. Mirrors the SQL rule in `marks.rs` so the
     * tree and the group list agree without a round-trip per row.
     */
    effectiveMark(
        node: Pick<TreeNode, 'id' | 'source_id' | 'rel_path'>,
    ): { mark: NodeMarkValue; explicit: boolean } | null {
        const own = this.marks.get(node.id);
        if (own) return { mark: own.mark, explicit: true };
        let best: NodeMark | null = null;
        for (const m of this.marks.values()) {
            if (m.source_id !== node.source_id || m.type !== 'directory') continue;
            if (!node.rel_path.startsWith(m.rel_path + '/')) continue;
            // Deepest ancestor wins: it is the most specific instruction.
            if (!best || m.rel_path.length > best.rel_path.length) best = m;
        }
        return best ? { mark: best.mark, explicit: false } : null;
    }

    private async setDedupStale(stale: boolean) {
        this.dedupStale = stale;
        await api.appStateSet('dedup_stale', stale ? '1' : '0');
    }

    async setView(view: ViewName) {
        this.view = view;
        await api.appStateSet('view', view);
    }

    async selectWorkspace(id: number) {
        this.activeWorkspaceId = id;
        // The scope points at a node in the workspace being left behind.
        this.scopeNode = null;
        this.folderReport = null;
        await api.appStateSet('active_workspace', String(id));
        await this.loadWorkspace();
    }

    async createWorkspace(name: string) {
        const ws = await api.workspaceCreate(name);
        this.workspaces = [...this.workspaces, ws];
        await this.selectWorkspace(ws.id);
    }

    async renameWorkspace(id: number, name: string) {
        await api.workspaceRename(id, name);
        this.workspaces = this.workspaces.map((w) =>
            w.id === id ? { ...w, name } : w,
        );
    }

    async deleteWorkspace(id: number) {
        await api.workspaceDelete(id);
        this.workspaces = this.workspaces.filter((w) => w.id !== id);
        if (this.activeWorkspaceId === id) {
            const next = this.workspaces[0];
            if (next) await this.selectWorkspace(next.id);
            else {
                this.activeWorkspaceId = null;
                this.sources = [];
                this.groups = [];
                this.deviceStats = [];
                this.scopeNode = null;
                this.folderReport = null;
                this.marks = new Map();
            }
        }
    }

    async importJson(jsonText: string, label: string) {
        if (this.activeWorkspaceId == null) return;
        await api.importTreeJson(this.activeWorkspaceId, jsonText, label);
        await this.setDedupStale(true);
        await this.loadWorkspace();
    }

    async scanFolder(path: string, label: string) {
        if (this.activeWorkspaceId == null) return;
        await api.scanFolder(this.activeWorkspaceId, path, label);
        await this.setDedupStale(true);
        await this.loadWorkspace();
    }

    async renameDevice(sourceId: number, label: string) {
        await api.sourceRenameDevice(sourceId, label);
        this.sources = this.sources.map((s) =>
            s.id === sourceId ? { ...s, device_label: label } : s,
        );
        this.deviceStats = this.deviceStats.map((d) =>
            d.source_id === sourceId ? { ...d, device_label: label } : d,
        );
    }

    async deleteSource(sourceId: number) {
        await api.sourceDelete(sourceId);
        await this.setDedupStale(true);
        await this.loadWorkspace();
    }

    async runDedup() {
        if (this.activeWorkspaceId == null) return;
        this.running = true;
        try {
            await api.appStateSet('min_size_kb', String(this.minSizeKb));
            await api.appStateSet('min_confidence', String(this.minConfidence));
            await api.runDedup(
                this.activeWorkspaceId,
                this.minSizeKb * 1024,
                this.minConfidence,
            );
            await this.setDedupStale(false);
            await this.loadWorkspace();
        } finally {
            this.running = false;
        }
    }

    /** Filters shared by every group query. */
    private groupQuery(offset: number) {
        return {
            workspaceId: this.activeWorkspaceId!,
            minConfidence: this.minConfidence,
            minSize: this.minSizeKb * 1024,
            kind: this.groupKind === 'all' ? undefined : this.groupKind,
            sort: this.groupSort,
            scopeNodeId: this.scopeNode?.id ?? null,
            decision: this.decisionFilter,
            offset,
            limit: GROUP_PAGE_SIZE,
        };
    }

    /** Reload the first page of groups using the current filters. */
    async refreshGroups() {
        if (this.activeWorkspaceId == null) return;
        this.groupsLoading = true;
        try {
            const page = await api.getGroups(this.groupQuery(0));
            this.groups = page.groups;
            this.groupTotal = page.total;
        } finally {
            this.groupsLoading = false;
        }
    }

    /** Load the next page of groups (infinite scroll). */
    async loadMoreGroups() {
        if (
            this.activeWorkspaceId == null ||
            this.groupsLoading ||
            this.groups.length >= this.groupTotal
        )
            return;
        this.groupsLoading = true;
        try {
            const page = await api.getGroups(this.groupQuery(this.groups.length));
            this.groups = [...this.groups, ...page.groups];
            this.groupTotal = page.total;
        } finally {
            this.groupsLoading = false;
        }
    }

    /** Reload only the groups already on screen, after a decision changed. */
    private async reloadCurrentGroups() {
        if (this.activeWorkspaceId == null) return;
        const shown = Math.max(this.groups.length, GROUP_PAGE_SIZE);
        const page = await api.getGroups({
            ...this.groupQuery(0),
            limit: Math.min(shown, 500),
        });
        this.groups = page.groups;
        this.groupTotal = page.total;
    }

    async setGroupSort(sort: GroupSort) {
        this.groupSort = sort;
        await this.refreshGroups();
    }

    async setGroupKind(kind: 'all' | 'file' | 'folder') {
        this.groupKind = kind;
        await this.refreshGroups();
    }

    async setDecisionFilter(decision: DecisionFilter) {
        this.decisionFilter = decision;
        await api.appStateSet('decision_filter', decision);
        await this.refreshGroups();
    }

    // --- Scope ---

    /** Focus the review panel on a node's subtree. */
    async setScope(node: ScopeTarget) {
        if (this.activeWorkspaceId == null) return;
        this.scopeNode = node;
        this.folderReport = null;
        this.reportLoading = true;
        // The group list does not depend on the report, so let both run at once.
        const groups = this.refreshGroups();
        try {
            this.folderReport = await api.getFolderReport(
                this.activeWorkspaceId,
                node.id,
            );
        } finally {
            this.reportLoading = false;
        }
        await groups;
    }

    async clearScope() {
        this.scopeNode = null;
        this.folderReport = null;
        await this.refreshGroups();
    }

    private async refreshReport() {
        if (this.activeWorkspaceId == null || !this.scopeNode) return;
        this.folderReport = await api.getFolderReport(
            this.activeWorkspaceId,
            this.scopeNode.id,
        );
    }

    // --- Keeper decisions ---

    /** Mark a node as the definitive copy, as surplus, or clear its decision. */
    async markNode(nodeId: number, mark: NodeMarkValue | null) {
        if (this.activeWorkspaceId == null) return;
        await api.setNodeMark(this.activeWorkspaceId, nodeId, mark);
        await this.loadMarks();
        await Promise.all([this.reloadCurrentGroups(), this.refreshReport()]);
    }

    /** Pick one member of a group as the keeper; `null` clears the decision. */
    async setGroupKeeper(groupId: number, nodeId: number | null) {
        if (this.activeWorkspaceId == null) return;
        await api.setGroupKeeper(this.activeWorkspaceId, groupId, nodeId);
        await this.loadMarks();
        await Promise.all([this.reloadCurrentGroups(), this.refreshReport()]);
    }

    /** Clear every decision at or beneath a node. */
    async clearMarks(nodeId: number) {
        if (this.activeWorkspaceId == null) return;
        await api.clearMarks(this.activeWorkspaceId, nodeId);
        await this.loadMarks();
        await Promise.all([this.reloadCurrentGroups(), this.refreshReport()]);
    }

    async exportDatabase(path: string) {
        await api.dbExport(path);
    }

    /** Import an external database file, merging it in as new workspaces. */
    async importDatabase(path: string): Promise<ImportSummary> {
        const summary = await api.dbImport(path);
        this.workspaces = await api.workspaceList();
        if (summary.new_workspace_ids.length > 0) {
            await this.selectWorkspace(
                summary.new_workspace_ids[summary.new_workspace_ids.length - 1],
            );
        }
        return summary;
    }
}

export const app = new AppState();
