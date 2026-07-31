<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import type {
        DecisionFilter,
        FolderOverlap,
        GroupSort,
        MatchGroup,
        MatchMember,
        TreeNode,
    } from '$lib/types';
    import { confidenceTone, formatBytes, formatTime, pct } from '$lib/util';
    import DeviceTree from './DeviceTree.svelte';
    import SourceManager from './SourceManager.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Badge } from '$lib/components/ui/badge';
    import { Slider } from '$lib/components/ui/slider';
    import { Label } from '$lib/components/ui/label';
    import * as Alert from '$lib/components/ui/alert';
    import * as Card from '$lib/components/ui/card';
    import * as Empty from '$lib/components/ui/empty';
    import * as ToggleGroup from '$lib/components/ui/toggle-group';
    import { toast } from 'svelte-sonner';

    let sentinel = $state<HTMLElement | null>(null);
    let reviewPane = $state<HTMLElement | null>(null);

    // Re-query group filters once the sliders settle.
    let sliderTimer: ReturnType<typeof setTimeout> | undefined;
    function slidersChanged() {
        clearTimeout(sliderTimer);
        sliderTimer = setTimeout(() => app.refreshGroups(), 250);
    }

    /** Focus the review panel on a node's subtree. */
    async function onselect(node: TreeNode) {
        try {
            await app.setScope(node);
        } catch (err) {
            toast.error(String(err));
        }
    }

    /** "Locate duplicates" from a tree row: scope, then reveal the panel. */
    async function onlocate(node: TreeNode) {
        await onselect(node);
        reviewPane?.scrollTo({ top: 0, behavior: 'smooth' });
    }

    /** Jump the scope to an overlapping folder, following the duplicates. */
    async function scopeToOverlap(o: FolderOverlap) {
        try {
            await app.setScope({
                id: o.node_id,
                source_id: o.source_id,
                name: o.name,
                rel_path: o.rel_path,
                type: 'directory',
            });
            reviewPane?.scrollTo({ top: 0, behavior: 'smooth' });
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function runDedup() {
        try {
            await app.runDedup();
            toast.success('Analysis complete.');
        } catch (err) {
            toast.error(String(err));
        }
    }

    /** Star toggles: starring the current keeper clears the decision. */
    async function toggleKeeper(g: MatchGroup, m: MatchMember) {
        try {
            await app.setGroupKeeper(
                g.id,
                g.keeper_node_id === m.node_id ? null : m.node_id
            );
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function keepWholeScope() {
        if (!app.scopeNode) return;
        try {
            await app.markNode(app.scopeNode.id, 'keep');
            toast.success(
                `Copies under “${app.scopeNode.name}” are now the ones to keep.`
            );
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function clearScopeDecisions() {
        if (!app.scopeNode) return;
        try {
            await app.clearMarks(app.scopeNode.id);
            toast.success('Decisions cleared.');
        } catch (err) {
            toast.error(String(err));
        }
    }

    // Infinite scroll: load the next page when the sentinel becomes visible.
    $effect(() => {
        const el = sentinel;
        if (!el) return;
        const obs = new IntersectionObserver(entries => {
            if (entries.some(e => e.isIntersecting)) app.loadMoreGroups();
        });
        obs.observe(el);
        return () => obs.disconnect();
    });

    const kinds: { id: 'all' | 'file' | 'folder'; label: string }[] = [
        { id: 'all', label: 'All' },
        { id: 'file', label: 'Files' },
        { id: 'folder', label: 'Folders' },
    ];
    const sorts: { id: GroupSort; label: string }[] = [
        { id: 'confidence', label: 'Confidence' },
        { id: 'size', label: 'Size' },
    ];
    const decisions: { id: DecisionFilter; label: string; title: string }[] = [
        { id: 'all', label: 'All', title: 'Every group' },
        {
            id: 'undecided',
            label: 'To decide',
            title: 'Groups with no definitive copy chosen yet',
        },
        {
            id: 'decided',
            label: 'Decided',
            title: 'Groups where a definitive copy is chosen',
        },
        {
            id: 'conflict',
            label: 'Conflicts',
            title: 'Groups with more than one copy marked to keep',
        },
    ];
</script>

<div class="grid min-h-0 grow grid-cols-[280px_1fr] gap-4 overflow-hidden">
    <!-- Left rail: device / source management -->
    <aside class="min-h-0 overflow-y-auto pe-1">
        <SourceManager />
    </aside>

    <div class="flex min-h-0 min-w-0 flex-col gap-6 overflow-hidden">
        {#if app.dedupStale}
            <Alert.Root class="flex-row items-center gap-3">
                <Icon icon="ph:warning-fill" class="text-warn" />
                <Alert.Description class="grow">
                    Sources changed since the last analysis — results may be out
                    of date.
                </Alert.Description>
                <Button size="sm" disabled={app.running} onclick={runDedup}>
                    {app.running ? 'Analyzing…' : 'Re-run Analysis'}
                </Button>
            </Alert.Root>
        {/if}

        <!-- Tuning controls -->
        <div class="flex flex-wrap items-end gap-6">
            <div class="flex min-w-48 flex-col gap-1.5">
                <Label for="minsize" class="text-xs text-muted-foreground">
                    Min file size:
                    <strong class="font-heading text-foreground tabular-nums"
                        >{app.minSizeKb} KB</strong>
                </Label>
                <Slider
                    id="minsize"
                    type="single"
                    min={0}
                    max={10240}
                    step={4}
                    bind:value={app.minSizeKb}
                    onValueChange={slidersChanged} />
                <span class="text-[0.7rem] text-muted-foreground">
                    Matches below this size are hidden
                </span>
            </div>
            <div class="flex min-w-48 flex-col gap-1.5">
                <Label for="minconf" class="text-xs text-muted-foreground">
                    Min confidence:
                    <strong class="font-heading text-foreground tabular-nums"
                        >{app.minConfidence}%</strong>
                </Label>
                <Slider
                    id="minconf"
                    type="single"
                    min={0}
                    max={100}
                    step={5}
                    bind:value={app.minConfidence}
                    onValueChange={slidersChanged} />
                <span class="text-[0.7rem] text-muted-foreground">
                    Hide weaker matches
                </span>
            </div>
            <Button disabled={app.running} onclick={runDedup}>
                <Icon icon="ph:magnifying-glass-bold" />
                <span>{app.running ? 'Analyzing…' : 'Find Duplicates'}</span>
            </Button>
        </div>

        <!-- Work area: trees + duplicate review -->
        <div class="grid min-h-0 grow grid-cols-2 gap-4 overflow-hidden">
            <div class="flex min-h-0 min-w-0 flex-col overflow-hidden pe-1">
                <h2 class="section-label mb-2 shrink-0">Device trees</h2>
                {#if app.sources.length === 0}
                    <Empty.Root class="border border-dashed">
                        <Empty.Header>
                            <Empty.Media variant="icon">
                                <Icon icon="ph:tree-structure-fill" />
                            </Empty.Media>
                            <Empty.Title>No devices</Empty.Title>
                            <Empty.Description>
                                Add devices from the left to browse their trees.
                            </Empty.Description>
                        </Empty.Header>
                    </Empty.Root>
                {:else}
                    {#each app.sources as s (s.id)}
                        <DeviceTree
                            source={s}
                            {onselect}
                            {onlocate}
                            selectedId={app.scopeNode?.id ?? null} />
                    {/each}
                {/if}
            </div>

            <aside
                class="flex min-h-0 min-w-0 flex-col overflow-x-hidden overflow-y-auto pe-1"
                bind:this={reviewPane}>
                <div class="mb-2 flex flex-wrap items-center gap-2">
                    <h2 class="section-label truncate">
                        {#if app.scopeNode}
                            Groups in {app.scopeNode.name}
                        {:else}
                            Duplicate groups
                        {/if}
                        {#if app.groupTotal > 0}
                            <span class="text-brand">({app.groupTotal})</span>
                        {/if}
                    </h2>
                    <div class="ms-auto flex flex-wrap items-center gap-3">
                        <ToggleGroup.Root
                            type="single"
                            size="sm"
                            variant="outline"
                            value={app.groupKind}
                            onValueChange={v =>
                                v &&
                                app.setGroupKind(
                                    v as 'all' | 'file' | 'folder'
                                )}>
                            {#each kinds as k (k.id)}
                                <ToggleGroup.Item
                                    value={k.id}
                                    class="px-2 text-xs">
                                    {k.label}
                                </ToggleGroup.Item>
                            {/each}
                        </ToggleGroup.Root>
                        <ToggleGroup.Root
                            type="single"
                            size="sm"
                            variant="outline"
                            value={app.groupSort}
                            onValueChange={v =>
                                v && app.setGroupSort(v as GroupSort)}>
                            {#each sorts as s (s.id)}
                                <ToggleGroup.Item
                                    value={s.id}
                                    class="px-2 text-xs">
                                    {s.label}
                                </ToggleGroup.Item>
                            {/each}
                        </ToggleGroup.Root>
                        <ToggleGroup.Root
                            type="single"
                            size="sm"
                            variant="outline"
                            value={app.decisionFilter}
                            onValueChange={v =>
                                v &&
                                app.setDecisionFilter(v as DecisionFilter)}>
                            {#each decisions as d (d.id)}
                                <ToggleGroup.Item
                                    value={d.id}
                                    title={d.title}
                                    class="px-2 text-xs">
                                    {d.label}
                                </ToggleGroup.Item>
                            {/each}
                        </ToggleGroup.Root>
                    </div>
                </div>

                <!-- Scope summary: what is duplicated in the selected subtree,
                     and where the counterparts live. -->
                {#if app.scopeNode}
                    {@const r = app.folderReport}
                    <Card.Root class="mb-3 gap-2 border-brand/70 py-3">
                        <Card.Header class="gap-1 px-3">
                            <Card.Title class="flex items-center gap-2 text-sm">
                                <Icon
                                    icon={app.scopeNode.type === 'directory'
                                        ? 'ph:folder-fill'
                                        : 'ph:file-fill'}
                                    class="shrink-0 text-brand" />
                                <span
                                    class="truncate"
                                    title={app.scopeNode.rel_path}>
                                    {app.scopeNode.name}
                                </span>
                                <Button
                                    variant="ghost"
                                    size="icon"
                                    class="ms-auto size-6"
                                    aria-label="Clear scope"
                                    title="Show all duplicate groups again"
                                    onclick={() => app.clearScope()}>
                                    <Icon icon="ph:x-bold" />
                                </Button>
                            </Card.Title>
                            <Card.Description class="text-xs">
                                {#if r}
                                    {r.device_label} · {r.file_count} files · {formatBytes(
                                        r.total_size
                                    )}
                                    {#if r.dup_bytes > 0}
                                        · <span class="text-warn"
                                            >{formatBytes(r.dup_bytes)} duplicated</span>
                                    {/if}
                                {:else if app.reportLoading}
                                    Analyzing…
                                {/if}
                            </Card.Description>
                        </Card.Header>
                        {#if r}
                            <Card.Content class="flex flex-col gap-3 px-3">
                                {#if r.group_count === 0}
                                    <p class="text-xs text-muted-foreground">
                                        Nothing here matches anything else in
                                        this workspace.
                                    </p>
                                {:else}
                                    <!-- The question a folder selection is
                                         really asking: where are the other
                                         copies? -->
                                    {#if r.overlaps.length > 0}
                                        <div class="flex flex-col gap-1">
                                            <span class="section-label"
                                                >Overlaps with</span>
                                            <ul class="flex flex-col">
                                                {#each r.overlaps as o (o.node_id)}
                                                    <li>
                                                        <button
                                                            type="button"
                                                            class="flex w-full items-center gap-2 rounded-sm px-1 py-0.5 text-start text-xs hover:bg-accent"
                                                            title="Focus {o.rel_path}"
                                                            onclick={() =>
                                                                scopeToOverlap(
                                                                    o
                                                                )}>
                                                            <span
                                                                class="shrink-0 font-medium whitespace-nowrap text-muted-foreground"
                                                                >{o.device_label}</span>
                                                            <span
                                                                class="flex-1 truncate"
                                                                >{o.rel_path}</span>
                                                            <span
                                                                class="shrink-0 font-heading tabular-nums"
                                                                >{formatBytes(
                                                                    o.shared_bytes
                                                                )}</span>
                                                            <span
                                                                class="shrink-0 whitespace-nowrap text-muted-foreground"
                                                                >{o.shared_files}
                                                                files</span>
                                                        </button>
                                                    </li>
                                                {/each}
                                            </ul>
                                        </div>
                                    {/if}

                                    <div
                                        class="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
                                        <span class="font-heading tabular-nums">
                                            {r.group_count} groups
                                        </span>
                                        <span
                                            class="font-heading tabular-nums text-ok">
                                            {r.decided_count} decided
                                        </span>
                                        <span class="font-heading tabular-nums">
                                            {r.group_count -
                                                r.decided_count -
                                                r.conflict_count} to decide
                                        </span>
                                        {#if r.conflict_count > 0}
                                            <span
                                                class="font-heading tabular-nums text-warn">
                                                {r.conflict_count} conflicting
                                            </span>
                                        {/if}
                                    </div>

                                    <!-- One mark on the folder settles every
                                         group beneath it; this is the whole
                                         point of subtree inheritance. -->
                                    <div class="flex flex-wrap gap-2">
                                        <Button
                                            size="sm"
                                            variant="outline"
                                            class="text-xs"
                                            onclick={keepWholeScope}>
                                            <Icon icon="ph:star-fill" />
                                            Keep copies here
                                        </Button>
                                        <Button
                                            size="sm"
                                            variant="ghost"
                                            class="text-xs text-muted-foreground"
                                            onclick={clearScopeDecisions}>
                                            Reset decisions
                                        </Button>
                                    </div>
                                {/if}
                            </Card.Content>
                        {/if}
                    </Card.Root>
                {/if}

                {#if app.groups.length === 0 && !app.groupsLoading}
                    <Empty.Root class="border border-dashed">
                        <Empty.Header>
                            <Empty.Media variant="icon">
                                <Icon icon="ph:copy-simple-fill" />
                            </Empty.Media>
                            <Empty.Title>
                                {#if app.scopeNode}
                                    Nothing to review here
                                {:else}
                                    No duplicate groups
                                {/if}
                            </Empty.Title>
                            <Empty.Description>
                                {#if app.scopeNode && app.decisionFilter !== 'all'}
                                    No “{decisions.find(
                                        d => d.id === app.decisionFilter
                                    )?.label}” groups inside {app.scopeNode
                                        .name}.
                                {:else if app.scopeNode}
                                    Nothing inside {app.scopeNode.name} matches anything
                                    else in this workspace.
                                {:else}
                                    Adjust the sliders and run “Find
                                    Duplicates”.
                                {/if}
                            </Empty.Description>
                        </Empty.Header>
                    </Empty.Root>
                {:else}
                    <ul class="flex flex-col gap-2">
                        {#each app.groups as g (g.id)}
                            <li
                                class="overflow-hidden rounded-md border transition-colors data-[folder=true]:border-brand/40 data-[folder=true]:bg-brand/[0.04]"
                                data-folder={g.kind === 'folder'}>
                                <div
                                    class="flex items-center gap-2 bg-muted/50 px-2 py-1 text-xs">
                                    <!-- Only folder groups carry the brand tint;
                                         they're the high-leverage matches. -->
                                    <Icon
                                        icon={g.kind === 'folder'
                                            ? 'ph:folder-fill'
                                            : 'ph:file-fill'}
                                        class="shrink-0 {g.kind === 'folder'
                                            ? 'text-brand'
                                            : 'text-muted-foreground'}" />
                                    <span
                                        class="font-heading font-semibold tabular-nums {confidenceTone(
                                            pct(g.confidence)
                                        )}">{pct(g.confidence)}%</span>
                                    <span class="truncate text-muted-foreground"
                                        >{g.primary_signal}</span>
                                    <span
                                        class="ms-auto shrink-0 font-heading tabular-nums"
                                        >{formatBytes(g.size)}</span>
                                    {#if g.decision === 'conflict'}
                                        <span
                                            class="inline-flex shrink-0 text-warn"
                                            title="More than one copy is marked to keep">
                                            <Icon icon="ph:warning-fill" />
                                        </span>
                                    {:else if g.decision === 'decided'}
                                        <span
                                            class="inline-flex shrink-0 text-ok"
                                            title="A definitive copy is chosen">
                                            <Icon icon="ph:check-circle-fill" />
                                        </span>
                                    {/if}
                                    <Badge
                                        variant="secondary"
                                        class="shrink-0 px-1.5 py-0 font-heading text-[0.65rem]">
                                        {g.members.length}×
                                    </Badge>
                                </div>
                                <ul class="flex flex-col py-0.5">
                                    {#each g.members as m (m.node_id)}
                                        <li
                                            class="flex items-center gap-2 px-2 py-0.5 text-xs data-[surplus=true]:opacity-50"
                                            data-surplus={g.decision ===
                                                'decided' &&
                                                g.keeper_node_id !== m.node_id}>
                                            <!-- The one action this view
                                                 produces: name the copy worth
                                                 keeping. -->
                                            <button
                                                type="button"
                                                class="shrink-0 text-muted-foreground hover:text-brand data-[keeper=true]:text-brand"
                                                data-keeper={m.mark === 'keep'}
                                                title={g.keeper_node_id ===
                                                m.node_id
                                                    ? 'Clear this decision'
                                                    : 'Keep this copy'}
                                                aria-label={g.keeper_node_id ===
                                                m.node_id
                                                    ? 'Clear this decision'
                                                    : 'Keep this copy'}
                                                onclick={() =>
                                                    toggleKeeper(g, m)}>
                                                <Icon
                                                    icon={m.mark === 'keep'
                                                        ? 'ph:star-fill'
                                                        : 'ph:star-bold'} />
                                            </button>
                                            {#if app.scopeNode}
                                                <span
                                                    class="w-8 shrink-0 font-heading text-[0.6rem] tracking-wide {m.in_scope
                                                        ? 'text-brand'
                                                        : 'text-muted-foreground'}">
                                                    {m.in_scope
                                                        ? 'HERE'
                                                        : 'OUT'}
                                                </span>
                                            {/if}
                                            <span
                                                class="shrink-0 font-medium whitespace-nowrap text-muted-foreground"
                                                >{m.device_label}</span>
                                            <span
                                                class="flex-1 truncate"
                                                class:line-through={g.decision ===
                                                    'decided' &&
                                                    g.keeper_node_id !==
                                                        m.node_id}
                                                title={m.rel_path}
                                                >{m.rel_path}</span>
                                            <span
                                                class="shrink-0 whitespace-nowrap text-muted-foreground">
                                                {formatTime(m.mtime)}
                                            </span>
                                        </li>
                                    {/each}
                                </ul>
                            </li>
                        {/each}
                    </ul>
                    <div
                        class="py-3 text-center text-xs text-muted-foreground"
                        bind:this={sentinel}>
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
        </div>
    </div>
</div>
