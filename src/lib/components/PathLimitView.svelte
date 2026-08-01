<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { PathTreeNode } from '$lib/types';
    import PathTreeItem from './PathTreeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Slider } from '$lib/components/ui/slider';
    import { Label } from '$lib/components/ui/label';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as Empty from '$lib/components/ui/empty';
    import { toast } from 'svelte-sonner';

    let limit = $state(260);
    let nodes = $state<PathTreeNode[]>([]);
    let loading = $state(false);

    async function reload() {
        if (app.activeWorkspaceId == null) return;
        loading = true;
        try {
            nodes = await api.pathfixTree(app.activeWorkspaceId, limit);
        } catch (err) {
            toast.error(String(err));
        } finally {
            loading = false;
        }
    }

    // Debounced: unlike the old over-limit-leaves-only endpoint, this one
    // returns the whole tree, and the slider can fire ~32 distinct values
    // while being dragged.
    let debounceHandle: ReturnType<typeof setTimeout> | undefined;
    let loadedFor = $state<string | null>(null);
    $effect(() => {
        const key = `${app.activeWorkspaceId}:${limit}`;
        if (app.activeWorkspaceId != null && key !== loadedFor) {
            loadedFor = key;
            clearTimeout(debounceHandle);
            debounceHandle = setTimeout(reload, 150);
        }
        return () => clearTimeout(debounceHandle);
    });

    // Indexed view over the flat node list: parent -> children, in the
    // order the backend already emitted them (its own sort_order walk).
    const index = $derived.by(() => {
        const byParent = new Map<number | null, PathTreeNode[]>();
        for (const n of nodes) {
            const bucket = byParent.get(n.parent_id) ?? [];
            bucket.push(n);
            byParent.set(n.parent_id, bucket);
        }
        return byParent;
    });

    function childrenOf(parentId: number | null): PathTreeNode[] {
        return index.get(parentId) ?? [];
    }

    async function handleRename(node: PathTreeNode, newName: string) {
        if (app.activeWorkspaceId == null) return;
        const name = newName.trim();
        if (!name || name === node.name) return;
        try {
            await api.pathfixRename(app.activeWorkspaceId, node.id, name);
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function handleRevert(node: PathTreeNode) {
        if (app.activeWorkspaceId == null) return;
        try {
            await api.pathfixRename(app.activeWorkspaceId, node.id, '');
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    const overCount = $derived(
        nodes.filter(n => childrenOf(n.id).length === 0 && n.path_length > limit)
            .length
    );
</script>

<div class="flex min-h-0 grow flex-col gap-3 overflow-hidden">
    <div class="flex flex-wrap items-end gap-6">
        <div class="flex min-w-56 flex-col gap-1.5">
            <Label for="limit" class="text-xs">
                Path length limit: <strong>{limit}</strong>
            </Label>
            <Slider
                id="limit"
                type="single"
                min={80}
                max={400}
                step={10}
                bind:value={limit} />
        </div>
        <div class="flex items-baseline gap-2 text-sm text-muted-foreground">
            <span
                class="text-2xl font-bold"
                class:text-destructive={overCount > 0}
                class:text-ok={overCount === 0}>{overCount}</span>
            <span>branches over limit</span>
        </div>
        <Button variant="outline" disabled={loading} onclick={reload}>
            <Icon
                icon={loading ? 'ph:spinner-gap-fill' : 'ph:radar-fill'}
                class={loading ? 'animate-spin' : ''} />
            <span>{loading ? 'Scanning…' : 'Rescan'}</span>
        </Button>
    </div>

    {#if nodes.length === 0}
        <Empty.Root class="border border-dashed">
            <Empty.Header>
                <Empty.Media variant="icon">
                    <Icon
                        icon={loading
                            ? 'ph:spinner-gap-fill'
                            : 'ph:tree-structure-fill'}
                        class={loading ? 'animate-spin' : ''} />
                </Empty.Media>
                <Empty.Title>
                    {loading ? 'Scanning…' : 'No consolidated tree yet'}
                </Empty.Title>
                <Empty.Description>
                    {loading
                        ? 'Measuring the consolidated end-state tree.'
                        : 'Build a plan in Consolidate first, then come back here to fix path lengths.'}
                </Empty.Description>
            </Empty.Header>
        </Empty.Root>
    {:else}
        {#if overCount === 0}
            <div
                class="flex items-center gap-2 rounded-md border border-ok/40 bg-ok/[0.06] px-3 py-1.5 text-sm text-ok">
                <Icon icon="ph:check-circle-fill" />
                <span>Everything fits within the current limit.</span>
            </div>
        {/if}
        <ScrollArea class="min-h-0 grow">
            <div class="pe-2" role="tree" aria-label="Consolidated tree">
                {#each childrenOf(null) as node (node.id)}
                    <PathTreeItem
                        {node}
                        {childrenOf}
                        {limit}
                        onrename={handleRename}
                        onrevert={handleRevert} />
                {/each}
            </div>
        </ScrollArea>
    {/if}
</div>
