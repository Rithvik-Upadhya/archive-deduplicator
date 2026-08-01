// Central application state using Svelte 5 runes. A single shared instance is
// exported; components import it and read/mutate its reactive fields. All
// mutations that matter to persistence are pushed to the Rust/SQLite backend.

import * as api from '../api';
import type {
    DeviceStats,
    GroupSort,
    ImportSummary,
    MatchGroup,
    Source,
    ViewName,
    Workspace,
} from '../types';

const GROUP_PAGE_SIZE = 50;

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
        await this.refreshGroups();
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

    /** Reload the first page of groups using the current filters. */
    async refreshGroups() {
        if (this.activeWorkspaceId == null) return;
        this.groupsLoading = true;
        try {
            const page = await api.getGroups({
                workspaceId: this.activeWorkspaceId,
                minConfidence: this.minConfidence,
                minSize: this.minSizeKb * 1024,
                kind: this.groupKind === 'all' ? undefined : this.groupKind,
                sort: this.groupSort,
                offset: 0,
                limit: GROUP_PAGE_SIZE,
            });
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
            const page = await api.getGroups({
                workspaceId: this.activeWorkspaceId,
                minConfidence: this.minConfidence,
                minSize: this.minSizeKb * 1024,
                kind: this.groupKind === 'all' ? undefined : this.groupKind,
                sort: this.groupSort,
                offset: this.groups.length,
                limit: GROUP_PAGE_SIZE,
            });
            this.groups = [...this.groups, ...page.groups];
            this.groupTotal = page.total;
        } finally {
            this.groupsLoading = false;
        }
    }

    async setGroupSort(sort: GroupSort) {
        this.groupSort = sort;
        await this.refreshGroups();
    }

    async setGroupKind(kind: 'all' | 'file' | 'folder') {
        this.groupKind = kind;
        await this.refreshGroups();
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
