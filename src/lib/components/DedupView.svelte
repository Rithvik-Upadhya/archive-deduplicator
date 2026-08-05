<script lang="ts">
    import { getGroupForNode } from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import type {
        DedupProgress,
        GroupSort,
        MatchGroup,
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
    import * as Resizable from '$lib/components/ui/resizable/index.js';
    import * as ToggleGroup from '$lib/components/ui/toggle-group';
    import { listen } from '@tauri-apps/api/event';

    let selectedNode = $state<TreeNode | null>(null);
    let selectedGroup = $state<MatchGroup | null>(null);
    let sentinel = $state<HTMLElement | null>(null);
    let reviewPane = $state<HTMLElement | null>(null);

    // Re-query group filters once the sliders settle, and persist the new
    // tuning + flag results stale -- min_size in particular is a hard filter
    // baked into the last `run_dedup` pass, so a lower value here can only
    // be reflected in the results after a rerun.
    let sliderTimer: ReturnType<typeof setTimeout> | undefined;
    function slidersChanged() {
        clearTimeout(sliderTimer);
        sliderTimer = setTimeout(() => {
            Promise.all([
                app.refreshGroups(),
                app.saveTuning(),
                app.setDedupStale(true),
            ]).catch(err =>
                taskTray.notify('Failed to update tuning', 'error', String(err))
            );
        }, 250);
    }

    async function onselect(node: TreeNode) {
        selectedNode = node;
        try {
            const group = await getGroupForNode(node.id);
            if (selectedNode?.id === node.id) selectedGroup = group;
        } catch (err) {
            if (selectedNode?.id === node.id) {
                taskTray.notify('Failed to load duplicates', 'error', String(err));
            }
        }
    }

    /** "Locate duplicate" from a tree row: select, then reveal the review panel. */
    async function onlocate(node: TreeNode) {
        await onselect(node);
        reviewPane?.scrollTo({ top: 0, behavior: 'smooth' });
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

    const kinds: { id: 'all' | 'file' | 'folder' | 'hardlink'; label: string }[] = [
        { id: 'all', label: 'All' },
        { id: 'file', label: 'Files' },
        { id: 'folder', label: 'Folders' },
        { id: 'hardlink', label: 'Hardlinks' },
    ];
    const sorts: { id: GroupSort; label: string }[] = [
        { id: 'confidence', label: 'Confidence' },
        { id: 'size', label: 'Size' },
    ];
</script>

<!-- Top row: Source management -->
<SourceManager />
<div class="grid min-h-0 grow grid-cols-[1fr] gap-4 overflow-hidden mt-8">
    <div class="flex min-h-0 min-w-0 flex-col gap-6 overflow-hidden">
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
            {#if app.dedupStale}
                <Alert.Root class="flex-row items-center gap-1.5 w-auto">
                    <Alert.Description>
                        <span class="flex flex-row items-center gap-2">
                            <Icon icon="ph:warning-fill" class="text-warn" />
                            Results may be out of date.
                        </span>
                    </Alert.Description>
                    <Button
                        size="sm"
                        disabled={taskTray.hasActive('dedup')}
                        onclick={runDedup}>
                        {taskTray.hasActive('dedup')
                            ? 'Analyzing…'
                            : 'Re-run Analysis'}
                    </Button>
                </Alert.Root>
            {:else}
                <Button
                    disabled={taskTray.hasActive('dedup')}
                    onclick={runDedup}>
                    <Icon icon="ph:magnifying-glass-bold" />
                    <span
                        >{taskTray.hasActive('dedup')
                            ? 'Analyzing…'
                            : 'Find Duplicates'}</span>
                </Button>
            {/if}
        </div>

        <!-- Work area: trees + duplicate review -->
        <Resizable.PaneGroup
            direction="horizontal"
            class="grid min-h-0 grow grid-cols-2 overflow-hidden">
            <Resizable.Pane>
                <div class="flex h-full min-w-0 flex-col overflow-hidden pe-1">
                    <h2 class="section-label mb-2 shrink-0">Device trees</h2>
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
                            class="flex flex-col overflow-y-auto overflow-x-hidden">
                            {#each app.visibleSources as s (s.id)}
                                <DeviceTree source={s} {onselect} {onlocate} />
                            {/each}
                        </div>
                    {/if}
                </div>
            </Resizable.Pane>
            <Resizable.Handle class="mx-3" />
            <Resizable.Pane>
                <aside
                    class="flex h-full min-w-0 flex-col overflow-hidden pe-1"
                    bind:this={reviewPane}>
                    <div class="mb-2 flex flex-wrap items-center gap-2">
                        <h2 class="section-label">
                            Duplicate groups
                            {#if app.groupTotal > 0}
                                <span class="text-brand"
                                    >({app.groupTotal})</span>
                            {/if}
                        </h2>
                        <div class="ms-auto flex items-center gap-3">
                            <ToggleGroup.Root
                                type="single"
                                size="sm"
                                variant="outline"
                                value={app.groupKind}
                                onValueChange={v =>
                                    v &&
                                    app.setGroupKind(
                                        v as 'all' | 'file' | 'folder' | 'hardlink'
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
                        </div>
                    </div>

                    <!-- Focused selection from the tree -->
                    {#if selectedGroup}
                        <Card.Root class="mb-3 gap-2 border-brand/70 py-3">
                            <Card.Header class="gap-1 px-3">
                                <Card.Title
                                    class="flex items-center gap-2 text-sm">
                                    <span class="truncate">
                                        Duplicates of: {selectedNode?.name}
                                    </span>
                                    <Button
                                        variant="ghost"
                                        size="icon"
                                        class="ms-auto size-6"
                                        aria-label="Close"
                                        onclick={() => {
                                            selectedGroup = null;
                                            selectedNode = null;
                                        }}>
                                        <Icon icon="ph:x-bold" />
                                    </Button>
                                </Card.Title>
                                <Card.Description class="text-xs">
                                    <span
                                        class="font-heading font-semibold tabular-nums {confidenceTone(
                                            pct(selectedGroup.confidence)
                                        )}">
                                        {pct(selectedGroup.confidence)}%
                                    </span>
                                    · {selectedGroup.primary_signal}
                                </Card.Description>
                            </Card.Header>
                            <Card.Content class="px-3">
                                <ul class="flex flex-col">
                                    {#each selectedGroup.members as m (m.node_id)}
                                        <li
                                            class="flex gap-2 rounded-sm px-1 py-0.5 text-xs data-[self=true]:bg-brand/10"
                                            data-self={m.node_id ===
                                                selectedNode?.id}>
                                            <span
                                                class="shrink-0 font-medium whitespace-nowrap text-muted-foreground"
                                                >{m.device_label}</span>
                                            <span
                                                class="flex-1 truncate"
                                                title={m.rel_path}
                                                >{m.rel_path}</span>
                                            <span
                                                class="shrink-0 whitespace-nowrap text-muted-foreground">
                                                {formatBytes(m.size)} · {formatTime(
                                                    m.mtime
                                                )}
                                            </span>
                                        </li>
                                    {/each}
                                </ul>
                            </Card.Content>
                        </Card.Root>
                    {:else if selectedNode}
                        <Card.Root class="mb-3 gap-1 py-3">
                            <Card.Header class="gap-1 px-3">
                                <Card.Title class="truncate text-sm"
                                    >{selectedNode.name}</Card.Title>
                                <Card.Description class="text-xs">
                                    No duplicate group for this file.
                                </Card.Description>
                            </Card.Header>
                        </Card.Root>
                    {/if}

                    {#if app.groups.length === 0 && !app.groupsLoading}
                        <Empty.Root class="border border-dashed">
                            <Empty.Header>
                                <Empty.Media variant="icon">
                                    <Icon icon="ph:copy-simple-fill" />
                                </Empty.Media>
                                <Empty.Title>No duplicate groups</Empty.Title>
                                <Empty.Description>
                                    Adjust the sliders and run “Find
                                    Duplicates”.
                                </Empty.Description>
                            </Empty.Header>
                        </Empty.Root>
                    {:else}
                        <div class="overflow-y-auto">
                            <ul class="flex flex-col gap-2">
                                {#each app.groups as g (g.id)}
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
                                            <span
                                                class="font-heading font-semibold tabular-nums {confidenceTone(
                                                    pct(g.confidence)
                                                )}">{pct(g.confidence)}%</span>
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
                                                    class="flex gap-2 px-2 py-0.5 text-xs">
                                                    <span
                                                        class="shrink-0 font-medium whitespace-nowrap text-muted-foreground"
                                                        >{m.device_label}</span>
                                                    <span
                                                        class="flex-1 truncate"
                                                        title={m.rel_path}
                                                        >{m.rel_path}</span>
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
