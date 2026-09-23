<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import type {
        DedupProgress,
        GroupKind,
        GroupSort,
        NodeType,
        TreeNode,
    } from '$lib/types';
    import {
        copyPath,
        formatBytes,
        GROUP_KINDS,
        MATCH_FLOORS,
        UNDERLINE_TRIGGER,
        type MatchTier,
    } from '$lib/util';
    import DeviceTree from './DeviceTree.svelte';
    import CollapseAllButton from './CollapseAllButton.svelte';
    import { deviceTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import GroupDialog from './GroupDialog.svelte';
    import RevealButton from './RevealButton.svelte';
    import MemberPath from './MemberPath.svelte';
    import { search } from '$lib/stores/search.svelte';
    import { TreeSelection } from '$lib/stores/selection.svelte';
    import SourceManager from './SourceManager.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import { Badge } from '$lib/components/ui/badge';
    import NumberField from './NumberField.svelte';
    import MatchFloorSelect from './MatchFloorSelect.svelte';
    import MatchTierLabel from './MatchTierLabel.svelte';
    import FilterSelect from './FilterSelect.svelte';
    import * as Select from '$lib/components/ui/select';
    import { Label } from '$lib/components/ui/label';
    import * as Alert from '$lib/components/ui/alert';
    import * as Empty from '$lib/components/ui/empty';
    import * as Resizable from '$lib/components/ui/resizable/index.js';
    import { listen } from '@tauri-apps/api/event';

    // Row clicks select, as in the Consolidate view's device trees; a member's
    // arrow in the group list or the dialog selects its row here too.
    const deviceSelection = new TreeSelection<{
        name: string;
        type: NodeType;
        source_id: number;
        cross_dup: boolean;
    }>();
    // The "locate duplicates" button's dialog.
    let groupOpen = $state(false);
    let groupNode = $state<TreeNode | null>(null);
    let sentinel = $state<HTMLElement | null>(null);

    // Selections the search might hide must not linger past it.
    let clearedFor = search.generation;
    $effect(() => {
        const gen = search.generation;
        if (gen === clearedFor) return;
        clearedFor = gen;
        deviceSelection.clear();
    });

    /** "Locate duplicates" from a tree row: show its group in a dialog. */
    function onlocate(node: TreeNode) {
        groupNode = node;
        groupOpen = true;
    }

    async function runDedup() {
        const taskId = taskTray.start('dedup', 'Finding duplicates…');
        let unlisten: (() => void) | undefined;
        try {
            unlisten = await listen<DedupProgress>('dedup:progress', e => {
                taskTray.update(taskId, {
                    phase: e.payload.phase,
                    current: e.payload.current,
                    total: e.payload.total,
                });
            });
            await app.runDedup();
            taskTray.resolve(taskId, 'success', 'Analysis complete.');
        } catch (err) {
            taskTray.resolve(taskId, 'error', String(err));
        } finally {
            unlisten?.();
        }
    }

    // Infinite scroll: load the next page when the sentinel becomes visible.
    $effect(() => {
        const el = sentinel;
        if (!el) return;
        const obs = new IntersectionObserver(entries => {
            if (entries.some(e => e.isIntersecting)) {
                app.loadMoreGroups().catch(err =>
                    taskTray.notify('Failed to load more results', 'error', String(err))
                );
            }
        });
        obs.observe(el);
        return () => obs.disconnect();
    });

    // Only the tiers the last run included: one below its floor can never
    // have groups, so it isn't offered at all.
    const tierOptions = $derived(
        MATCH_FLOORS.filter(f => f.confidence >= app.appliedMinConfidence).map(
            f => ({
                value: f.tier,
                label: `Tier ${f.tier}`,
                short: f.tier,
                hint: f.label,
            })
        )
    );

    // The menu only reports the tiers it shows, so a hidden tier keeps
    // whatever selection it had -- it comes back as it was if a later run
    // lowers the floor again.
    function setShownTiers(shown: MatchTier[]) {
        const offered = new Set(tierOptions.map(o => o.value));
        const hidden = app.groupTiers.filter(t => !offered.has(t));
        app.setGroupTiers(
            MATCH_FLOORS.map(f => f.tier).filter(
                t => shown.includes(t) || hidden.includes(t)
            )
        );
    }
    const sorts: { id: GroupSort; label: string }[] = [
        { id: 'tier-desc', label: 'Tier: A → E' },
        { id: 'tier-asc', label: 'Tier: E → A' },
        { id: 'size-desc', label: 'Size: largest first' },
        { id: 'size-asc', label: 'Size: smallest first' },
    ];
    const sortLabel = $derived(
        sorts.find(s => s.id === app.groupSort)?.label ?? ''
    );
    // A type or tier filter is narrowing the list.
    const listFiltered = $derived(
        app.groupKinds.length < GROUP_KINDS.length ||
            app.groupTiers.length < MATCH_FLOORS.length
    );

    // Sources changed since the last run, or a tuning field was edited and
    // not yet run (the fields are run parameters, not list filters).
    const resultsStale = $derived(app.dedupStale || app.tuningPending);
</script>

<!-- Top row: Source management -->
<SourceManager />
<div class="grid min-h-0 grow grid-cols-[1fr] gap-4 overflow-hidden mt-8">
    <div class="flex min-h-0 min-w-0 flex-col gap-6 overflow-hidden">
        <!-- Tuning controls -->
        <div class="flex flex-wrap items-end gap-6">
            <div class="flex min-w-48 flex-col gap-1.5">
                <Label for="minsize" class="text-xs text-muted-foreground">
                    Min file size
                </Label>
                <NumberField
                    id="minsize"
                    value={app.minSizeKb}
                    suffix="KB"
                    oncommit={v => (app.minSizeKb = v)} />
                <span class="text-[0.7rem] text-muted-foreground">
                    Only larger files are compared
                </span>
            </div>
            <div class="flex min-w-48 flex-col gap-1.5">
                <Label for="minconf" class="text-xs text-muted-foreground">
                    Weakest match type to include
                </Label>
                <MatchFloorSelect
                    id="minconf"
                    value={app.minConfidence}
                    oncommit={v => (app.minConfidence = v)} />
                <span class="text-[0.7rem] text-muted-foreground">
                    Stricter types are always included
                </span>
            </div>
            <!-- Always boxed, with the results' state above the button. A
                 persistent status rather than an alert, hence the role. -->
            <Alert.Root role="status" class="flex w-auto flex-col items-stretch gap-1.5">
                <Alert.Description>
                    <span class="flex flex-row items-center gap-2">
                        {#if resultsStale}
                            <Icon icon="ph:warning-fill" class="text-warn" />
                            Results are out of date.
                        {:else}
                            <Icon icon="ph:check-circle-fill" class="text-ok" />
                            Results are up to date.
                        {/if}
                    </span>
                </Alert.Description>
                <Button
                    size="sm"
                    disabled={taskTray.hasActive('dedup')}
                    onclick={runDedup}>
                    <Icon
                        icon={resultsStale
                            ? 'ph:arrow-counter-clockwise-bold'
                            : 'ph:magnifying-glass-bold'} />
                    <span
                        >{taskTray.hasActive('dedup')
                            ? 'Analyzing…'
                            : resultsStale
                              ? 'Re-run analysis'
                              : 'Find duplicates'}</span>
                </Button>
            </Alert.Root>
        </div>

        <!-- Work area: trees + duplicate review -->
        <Resizable.PaneGroup
            direction="horizontal"
            class="grid min-h-0 grow grid-cols-2 overflow-hidden">
            <Resizable.Pane>
                <div class="flex h-full min-w-0 flex-col overflow-hidden pe-1">
                    <div
                        class="mb-2 flex shrink-0 items-center justify-between gap-2">
                        <h2 class="section-label">Device trees</h2>
                        <CollapseAllButton
                            set={search.active
                                ? search.deviceExpanded
                                : deviceTreeExpanded} />
                    </div>
                    {#if app.visibleSources.length === 0}
                        <Empty.Root class="border border-dashed">
                            <Empty.Header>
                                <Empty.Media variant="icon">
                                    <Icon icon="ph:tree-structure-fill" />
                                </Empty.Media>
                                <Empty.Title>No devices</Empty.Title>
                                <Empty.Description>
                                    Add devices to browse their trees.
                                </Empty.Description>
                            </Empty.Header>
                        </Empty.Root>
                    {:else}
                        <div
                            data-device-list
                            class="flex flex-col overflow-y-auto overflow-x-hidden">
                            {#each app.visibleSources as s (s.id)}
                                <DeviceTree
                                    source={s}
                                    selection={deviceSelection}
                                    {onlocate} />
                            {/each}
                        </div>
                    {/if}
                </div>
            </Resizable.Pane>
            <Resizable.Handle class="mx-3" />
            <Resizable.Pane>
                <aside
                    class="flex h-full min-w-0 flex-col overflow-hidden pe-1">
                    <div class="mb-2 flex flex-wrap items-center gap-2">
                        <h2 class="section-label">
                            Duplicate groups
                            {#if app.groupTotal > 0}
                                <span class="text-brand"
                                    >({app.groupTotal})</span>
                            {/if}
                        </h2>
                        <div
                            class="ms-auto flex flex-wrap items-center gap-3">
                            <FilterSelect
                                ariaLabel="Item types"
                                options={GROUP_KINDS}
                                selected={app.groupKinds}
                                allText="All types"
                                onchange={v =>
                                    app.setGroupKinds(v as GroupKind[])} />
                            <FilterSelect
                                ariaLabel="Match tiers"
                                options={tierOptions}
                                selected={app.groupTiers}
                                allText="All tiers"
                                prefix="Tiers "
                                onchange={v =>
                                    setShownTiers(v as MatchTier[])} />
                            <Select.Root
                                type="single"
                                value={app.groupSort}
                                onValueChange={v =>
                                    v && app.setGroupSort(v as GroupSort)}>
                                <Select.Trigger
                                    size="sm"
                                    class="w-40 {UNDERLINE_TRIGGER}"
                                    aria-label="Sort">
                                    {sortLabel}
                                </Select.Trigger>
                                <Select.Content align="end">
                                    {#each sorts as s (s.id)}
                                        <Select.Item
                                            value={s.id}
                                            label={s.label} />
                                    {/each}
                                </Select.Content>
                            </Select.Root>
                        </div>
                    </div>

                    {#if app.groups.length === 0 && !app.groupsLoading}
                        <Empty.Root class="border border-dashed">
                            <Empty.Header>
                                <Empty.Media variant="icon">
                                    <Icon icon="ph:copy-simple-fill" />
                                </Empty.Media>
                                <Empty.Title>No duplicate groups</Empty.Title>
                                <Empty.Description>
                                    {#if search.applied}
                                        No group has a member whose name
                                        contains “{search.applied.query}”.
                                    {:else if listFiltered}
                                        No group matches the selected types
                                        and tiers.
                                    {:else}
                                        Adjust the settings and run “Find
                                        duplicates”.
                                    {/if}
                                </Empty.Description>
                            </Empty.Header>
                        </Empty.Root>
                    {:else}
                        <div class="overflow-y-auto">
                            <ul class="flex flex-col gap-2">
                                {#each app.groups as g (g.id)}
                                    {@const groupCopyLabel =
                                        g.kind === 'folder'
                                            ? 'Copy folder path'
                                            : 'Copy file path'}
                                    <li
                                        class="rounded-md border transition-colors data-[folder=true]:border-brand/40 data-[folder=true]:bg-brand/[0.04]"
                                        data-folder={g.kind === 'folder'}>
                                        <div
                                            class="flex items-center gap-2 bg-muted/50 px-2 py-1 text-xs">
                                            <!-- Only folder groups carry the brand tint;
                                             they're the high-leverage matches. -->
                                            <Icon
                                                icon={g.kind === 'folder'
                                                    ? 'ph:folder-fill'
                                                    : 'ph:file-fill'}
                                                class="shrink-0 {g.kind ===
                                                'folder'
                                                    ? 'text-brand'
                                                    : 'text-muted-foreground'}" />
                                            <MatchTierLabel
                                                confidence={g.confidence}
                                                kind={g.kind} />
                                            <span
                                                class="truncate text-muted-foreground"
                                                >{g.primary_signal}</span>
                                            <span
                                                class="ms-auto shrink-0 font-heading tabular-nums"
                                                >{formatBytes(g.size)}</span>
                                            <Badge
                                                variant="secondary"
                                                class="shrink-0 px-1.5 py-0 font-heading text-[0.65rem]">
                                                {g.members.length}×
                                            </Badge>
                                        </div>
                                        <ul class="flex flex-col py-0.5">
                                            {#each g.members as m (m.node_id)}
                                                <li
                                                    class="flex items-center gap-2 px-2 py-0.5 text-xs">
                                                    <!-- Yellow names the member the
                                                         search matched, in case the
                                                         highlight is truncated. -->
                                                    <span
                                                        class="shrink-0 font-medium whitespace-nowrap {search.matches(
                                                            m.name
                                                        )
                                                            ? 'text-hit-text'
                                                            : 'text-muted-foreground'}"
                                                        >{m.device_label}</span>
                                                    <span
                                                        class="flex-1 truncate"
                                                        title={m.rel_path}
                                                        ><MemberPath
                                                            relPath={m.rel_path}
                                                            name={m.name} /></span>
                                                    <Button
                                                        variant="ghost"
                                                        size="icon"
                                                        class="size-5 shrink-0 text-muted-foreground hover:text-foreground"
                                                        title={groupCopyLabel}
                                                        onclick={e => {
                                                            e.stopPropagation();
                                                            copyPath(
                                                                m.rel_path,
                                                                app.pathSep
                                                            );
                                                        }}>
                                                        <Icon
                                                            icon="ph:copy-fill" />
                                                        <span class="sr-only"
                                                            >{groupCopyLabel}</span>
                                                    </Button>
                                                    <RevealButton member={m} />
                                                </li>
                                            {/each}
                                        </ul>
                                    </li>
                                {/each}
                            </ul>
                            <div bind:this={sentinel}></div>
                        </div>
                        <div
                            class="py-3 text-center text-xs text-muted-foreground">
                            {#if app.groupsLoading}
                                Loading…
                            {:else if app.groups.length < app.groupTotal}
                                Scroll for more ({app.groups.length} / {app.groupTotal})
                            {:else if app.groupTotal > 0}
                                All {app.groupTotal} groups loaded
                            {/if}
                        </div>
                    {/if}
                </aside>
            </Resizable.Pane>
        </Resizable.PaneGroup>
    </div>
</div>

<GroupDialog bind:open={groupOpen} node={groupNode} />
