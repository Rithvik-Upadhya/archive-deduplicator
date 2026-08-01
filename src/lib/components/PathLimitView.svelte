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
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import { toast } from 'svelte-sonner';

    let limit = $state(260);
    let nodes = $state<PathTreeNode[]>([]);
    let loading = $state(false);
    let deleteTarget = $state<PathTreeNode | null>(null);

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
    // Plain (non-reactive) on purpose: it's only compared inside this same
    // effect, never read elsewhere. Making it $state would cause writing to
    // it here to re-trigger this very effect, and the cleanup below would
    // then cancel the just-scheduled reload before it ever fires.
    let loadedFor: string | null = null;
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

    /** BFS descendant closure (including the node itself) — used to block
     *  dropping a directory into its own descendant. */
    function descendantsOf(id: number): Set<number> {
        const set = new Set<number>([id]);
        let changed = true;
        while (changed) {
            changed = false;
            for (const n of nodes) {
                if (
                    n.parent_id != null &&
                    set.has(n.parent_id) &&
                    !set.has(n.id)
                ) {
                    set.add(n.id);
                    changed = true;
                }
            }
        }
        return set;
    }

    async function handleMoveWithin(nodeId: number, newParentId: number | null) {
        if (app.activeWorkspaceId == null) return;
        if (nodeId === newParentId) return;
        if (newParentId != null && descendantsOf(nodeId).has(newParentId)) {
            toast.error("Can't move a folder into itself.");
            return;
        }
        const newSortOrder =
            Math.max(0, ...childrenOf(newParentId).map(n => n.sort_order)) + 1;
        try {
            await api.consolidationMoveNode(nodeId, newParentId, newSortOrder);
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    function handleDrop(parentId: number | null, e: DragEvent) {
        const raw = e.dataTransfer?.getData('application/x-dedup-cons-node');
        if (!raw) return;
        const { id } = JSON.parse(raw) as { id: number };
        handleMoveWithin(id, parentId);
    }

    let rootDragOver = $state(false);

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

    async function confirmDelete() {
        const target = deleteTarget;
        deleteTarget = null;
        if (!target) return;
        try {
            await api.consolidationDeleteNode(target.id);
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
            <div
                class="min-h-full rounded-md border border-transparent pe-2 data-[drag=true]:border-brand data-[drag=true]:bg-brand/10"
                data-drag={rootDragOver}
                role="tree"
                aria-label="Consolidated tree"
                tabindex="0"
                ondragover={e => {
                    e.preventDefault();
                    rootDragOver = true;
                }}
                ondragleave={() => (rootDragOver = false)}
                ondrop={e => {
                    e.preventDefault();
                    rootDragOver = false;
                    handleDrop(null, e);
                }}>
                {#each childrenOf(null) as node (node.id)}
                    <PathTreeItem
                        {node}
                        {childrenOf}
                        {limit}
                        onrename={handleRename}
                        onrevert={handleRevert}
                        ondropInto={handleDrop}
                        ondelete={n => (deleteTarget = n)} />
                {/each}
            </div>
        </ScrollArea>
    {/if}
</div>

<AlertDialog.Root
    open={deleteTarget !== null}
    onOpenChange={o => {
        if (!o) deleteTarget = null;
    }}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Remove from the plan?</AlertDialog.Title>
            <AlertDialog.Description>
                “{deleteTarget?.name}”{deleteTarget?.type === 'directory'
                    ? ' and everything inside it'
                    : ''} will be removed from the consolidated tree. Your source
                devices are not touched.
            </AlertDialog.Description>
        </AlertDialog.Header>
        <AlertDialog.Footer>
            <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                onclick={confirmDelete}
                class="bg-destructive text-white hover:bg-destructive/90">
                Remove
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
