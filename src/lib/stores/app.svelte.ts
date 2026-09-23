// Central application state using Svelte 5 runes. A single shared instance is
// exported; components import it and read/mutate its reactive fields. All
// mutations that matter to persistence are pushed to the Rust/SQLite backend.

import * as api from '../api';
import { AUTO_EXPAND_LIMIT, search } from './search.svelte';
import { taskTray } from './tasks.svelte';
import {
    GROUP_KINDS,
    LOOSEST_MATCH_FLOOR,
    MATCH_FLOORS,
    snapMatchFloor,
    tierRange,
    type MatchTier,
} from '../util';
import type { IconName } from '../components/Icon.svelte';
import type {
    GroupKind,
    GroupSort,
    HashScanReportDto,
    ImportSummary,
    MatchGroup,
    Source,
    ViewName,
    Workspace,
} from '../types';

const GROUP_PAGE_SIZE = 50;

/** Navigation metadata shared by the sidebar and the breadcrumb. */
export const VIEWS: { id: ViewName; label: string; icon: IconName }[] = [
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

    // Dedup tuning parameters, as the inputs currently show them. They are run
    // parameters, not list filters: nothing uses them until `runDedup`.
    // 64 KB matches the matcher's default `min_size_bytes` floor: the vast
    // majority of files below this are noise-generating size collisions
    // (thumbnail caches, tiny fixed-size files) that account for a
    // negligible share of actual bytes on a typical archive.
    minSizeKb = $state(64);
    minConfidence = $state(LOOSEST_MATCH_FLOOR);

    // The values the last run used, which the group list filters by: the
    // groups, trees and stats all describe that run. Set only from storage
    // (`loadWorkspace`), and only a successful `runDedup` writes storage, so
    // an edit that was never run is dropped on reload or workspace switch.
    appliedMinSizeKb = $state(64);
    appliedMinConfidence = $state(LOOSEST_MATCH_FLOOR);

    /** An input differs from what the last run used (nudges a re-run). */
    tuningPending = $derived(
        this.minSizeKb !== this.appliedMinSizeKb ||
            this.minConfidence !== this.appliedMinConfidence,
    );

    // Results (paged: `groups` holds all pages loaded so far).
    groups = $state<MatchGroup[]>([]);
    groupTotal = $state(0);
    groupSort = $state<GroupSort>('tier-desc');
    // List filters, each "any of". Never empty: the setters refuse that, so
    // unchecking the last item can't silently mean "show everything".
    groupKinds = $state<GroupKind[]>(GROUP_KINDS.map(k => k.value));
    groupTiers = $state<MatchTier[]>(MATCH_FLOORS.map(f => f.tier));
    groupsLoading = $state(false);

    /** True when sources changed after the last dedup run (nudges a re-run). */
    dedupStale = $state(false);

    /** Bumped by whichever mutation can actually change tree shape or
     *  duplicate annotations (import, scan, delete, copy-to-workspace,
     *  dedup rerun), so tree panels know their cached `roots`/`children`
     *  (fetched via `getTree`) may be stale and should refetch. Switching
     *  workspaces or hashing doesn't touch this -- a workspace switch
     *  already remounts every device panel from scratch, and hashing alone
     *  (without a dedup rerun) doesn't change what `getTree` returns. */
    treeVersion = $state(0);

    /**
     * The host OS path separator, used when copying a folder path so it can be
     * pasted straight into the user's own file manager. Defaults to '/' until
     * `init()` has asked the backend.
     */
    pathSep = $state('/');

    loading = $state(true);

    /** Guards against `init()` running more than once. */
    private started = false;

    get activeWorkspace(): Workspace | null {
        return this.workspaces.find((w) => w.id === this.activeWorkspaceId) ?? null;
    }

    /** Sources not excluded from analysis -- what tree panels should render. */
    get visibleSources(): Source[] {
        return this.sources.filter((s) => !s.excluded);
    }

    async init() {
        if (this.started) return;
        this.started = true;
        this.loading = true;
        try {
            // Cosmetic, so it gets its own catch: failing to learn the
            // separator must not take the whole boot down with it. '/' stays.
            try {
                this.pathSep = await api.pathSeparator();
            } catch {
                /* keep the default */
            }
            this.workspaces = await api.workspaceList();
            if (this.workspaces.length === 0) {
                const ws = await api.workspaceCreate('Workspace 1');
                this.workspaces = [ws];
            }
            const [lastId, rawView] = await Promise.all([
                api.appStateGet('active_workspace'),
                api.appStateGet('view'),
            ]);
            const view = rawView as ViewName | null;
            const parsed = lastId ? Number(lastId) : null;
            this.activeWorkspaceId =
                parsed && this.workspaces.some((w) => w.id === parsed)
                    ? parsed
                    : this.workspaces[0].id;
            if (view) this.view = view;

            await this.loadWorkspace();
        } finally {
            this.loading = false;
        }
    }

    async loadWorkspace() {
        if (this.activeWorkspaceId == null) return;
        const ws = this.activeWorkspaceId;
        const [minSize, minConf, stale] = await Promise.all([
            api.workspaceStateGet(ws, 'min_size_kb'),
            api.workspaceStateGet(ws, 'min_confidence'),
            api.workspaceStateGet(ws, 'dedup_stale'),
        ]);
        this.appliedMinSizeKb = minSize ? Number(minSize) : 64;
        // Snapped onto a match type: older workspaces stored any number.
        this.appliedMinConfidence = snapMatchFloor(
            minConf ? Number(minConf) : LOOSEST_MATCH_FLOOR,
        );
        this.minSizeKb = this.appliedMinSizeKb;
        this.minConfidence = this.appliedMinConfidence;
        this.dedupStale = stale === '1';

        await Promise.all([
            api.sourceList(ws).then((sources) => (this.sources = sources)),
            this.refreshGroups(),
        ]);
    }

    /** Persist the tuning a run just used for the active workspace. Only
     *  `runDedup` calls this, after the run succeeds, so the stored values are
     *  always the applied ones. */
    private async saveTuning(minSizeKb: number, minConfidence: number) {
        if (this.activeWorkspaceId == null) return;
        const ws = this.activeWorkspaceId;
        await api.workspaceStateSet(ws, 'min_size_kb', String(minSizeKb));
        await api.workspaceStateSet(ws, 'min_confidence', String(minConfidence));
    }

    async setDedupStale(stale: boolean) {
        if (this.activeWorkspaceId == null) return;
        this.dedupStale = stale;
        await api.workspaceStateSet(
            this.activeWorkspaceId,
            'dedup_stale',
            stale ? '1' : '0',
        );
    }

    async setView(view: ViewName) {
        // Search state is per view: switching starts from a clean slate.
        if (view !== this.view) await this.clearSearch();
        this.view = view;
        await api.appStateSet('view', view);
    }

    async selectWorkspace(id: number) {
        // The applied search's node ids belong to the old workspace.
        search.reset();
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
            }
        }
    }

    async importJson(jsonText: string, label: string) {
        if (this.activeWorkspaceId == null) return;
        await api.importTreeJson(this.activeWorkspaceId, jsonText, label);
        await this.setDedupStale(true);
        await this.loadWorkspace();
        this.treeVersion++;
    }

    async scanFolder(
        path: string,
        label: string,
        hashingEnabled: boolean,
        hashMinSize?: number,
        mediumOverride?: string,
        filesystemOverride?: string,
    ): Promise<Source | null> {
        if (this.activeWorkspaceId == null) return null;
        const source = await api.scanFolder(
            this.activeWorkspaceId,
            path,
            label,
            hashingEnabled,
            hashMinSize,
            mediumOverride,
            filesystemOverride,
        );
        await this.setDedupStale(true);
        await this.loadWorkspace();
        this.treeVersion++;
        return source;
    }

    /** Run (or resume) the content-hashing pass for one source, then reload
     *  so per-source hash coverage stats reflect the new state. Flags dedup
     *  as stale whenever new hashes were actually produced -- true whether
     *  this call finished the pass or was cancelled partway through. */
    async runHashScan(sourceId: number): Promise<HashScanReportDto> {
        const report = await api.runHashScan(sourceId);
        if (report.hashed > 0) {
            await this.setDedupStale(true);
        }
        await this.loadWorkspace();
        return report;
    }

    async renameDevice(sourceId: number, label: string) {
        await api.sourceRenameDevice(sourceId, label);
        this.sources = this.sources.map((s) =>
            s.id === sourceId ? { ...s, device_label: label } : s,
        );
    }

    async setSourceColor(sourceId: number, color: string) {
        await api.sourceSetColor(sourceId, color);
        this.sources = this.sources.map((s) =>
            s.id === sourceId ? { ...s, color } : s,
        );
    }

    async setSourceExcluded(sourceId: number, excluded: boolean) {
        await api.sourceSetExcluded(sourceId, excluded);
        this.sources = this.sources.map((s) =>
            s.id === sourceId ? { ...s, excluded } : s,
        );
        await this.setDedupStale(true);
        // get_groups now hides any group without >=2 members on live sources,
        // so re-query the pane immediately -- the excluded source's groups
        // (including hardlink groups a re-run would not clear) drop out at
        // once. The dup-% badges still wait for the nudged re-run.
        await this.refreshGroups();
    }

    async copySourceToWorkspace(sourceId: number, targetWorkspaceId: number) {
        await api.sourceCopyToWorkspace(sourceId, targetWorkspaceId);
        if (targetWorkspaceId === this.activeWorkspaceId) {
            await this.setDedupStale(true);
            await this.loadWorkspace();
            this.treeVersion++;
        } else {
            // setDedupStale only writes for the active workspace -- the
            // target isn't active, so flag it directly so loadWorkspace
            // picks up dedup_stale='1' next time the user switches into it.
            await api.workspaceStateSet(targetWorkspaceId, 'dedup_stale', '1');
        }
    }

    async deleteSource(sourceId: number) {
        await api.sourceDelete(sourceId);
        await this.setDedupStale(true);
        await this.loadWorkspace();
        this.treeVersion++;
    }

    async runDedup() {
        if (this.activeWorkspaceId == null) return;
        // Snapshot before the first await: a run can take a while and the
        // inputs stay editable. Saved only once the run succeeds, so a failed
        // run can't leave values stored as if they had been applied.
        const minSizeKb = this.minSizeKb;
        const minConfidence = this.minConfidence;
        await api.runDedup(this.activeWorkspaceId, minSizeKb * 1024, minConfidence);
        await this.saveTuning(minSizeKb, minConfidence);
        await this.setDedupStale(false);
        // A new run is a new set of groups: start the list unfiltered, before
        // `loadWorkspace` fetches its first page.
        this.groupKinds = GROUP_KINDS.map(k => k.value);
        this.groupTiers = MATCH_FLOORS.map(f => f.tier);
        await this.loadWorkspace();
        this.treeVersion++;
    }

    /**
     * Run the top-bar search with the input's current text and case toggle.
     * The device trees filter by the result; the Deduplicate view's group
     * list is re-queried with it. The consolidated tree filters itself from
     * `search.applied`. An empty query clears the search instead.
     */
    async runSearch(query = search.query, caseSensitive = search.caseSensitive) {
        if (this.activeWorkspaceId == null) return;
        if (query === '') return this.clearSearch();
        search.busy = true;
        try {
            const res = await api.searchNodes(
                this.activeWorkspaceId,
                query,
                caseSensitive,
            );
            search.apply(query, caseSensitive, res);
            if (res.matched.length > AUTO_EXPAND_LIMIT) {
                taskTray.notify(
                    `${res.matched.length} matches`,
                    'success',
                    'Too many to open every folder at once -- expand folders to browse them.',
                );
            }
            if (this.view === 'dedup') await this.refreshGroups();
        } catch (err) {
            taskTray.notify('Search failed', 'error', String(err));
        } finally {
            search.busy = false;
        }
    }

    /** Leave the searching state, restoring the unfiltered group list. */
    async clearSearch() {
        const wasSearching = search.active;
        search.reset();
        if (wasSearching && this.view === 'dedup') await this.refreshGroups();
    }

    /** Group-list arguments for the applied search, if any. */
    private get groupSearchArgs() {
        const a = search.applied;
        return a ? { search: a.query, caseSensitive: a.caseSensitive } : {};
    }

    /** Group-list arguments for the type and tier filters. Each is sent only
     *  when it actually narrows the list. */
    private get groupFilterArgs() {
        return {
            kinds:
                this.groupKinds.length < GROUP_KINDS.length
                    ? this.groupKinds
                    : undefined,
            tiers:
                this.groupTiers.length < MATCH_FLOORS.length
                    ? this.groupTiers.map(tierRange)
                    : undefined,
        };
    }

    /** Reload the first page of groups using the current filters. */
    async refreshGroups() {
        if (this.activeWorkspaceId == null) return;
        this.groupsLoading = true;
        try {
            const page = await api.getGroups({
                workspaceId: this.activeWorkspaceId,
                minConfidence: this.appliedMinConfidence,
                minSize: this.appliedMinSizeKb * 1024,
                sort: this.groupSort,
                ...this.groupFilterArgs,
                ...this.groupSearchArgs,
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
                minConfidence: this.appliedMinConfidence,
                minSize: this.appliedMinSizeKb * 1024,
                sort: this.groupSort,
                ...this.groupFilterArgs,
                ...this.groupSearchArgs,
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

    async setGroupKinds(kinds: GroupKind[]) {
        if (kinds.length === 0) return;
        this.groupKinds = kinds;
        await this.refreshGroups();
    }

    async setGroupTiers(tiers: MatchTier[]) {
        if (tiers.length === 0) return;
        this.groupTiers = tiers;
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
