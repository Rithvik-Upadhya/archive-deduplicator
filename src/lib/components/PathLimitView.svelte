<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { PathTreeNode } from '$lib/types';
    import { TreeSelection } from '$lib/stores/selection.svelte';
    import PathTreeItem from './PathTreeItem.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { pathSegments } from '$lib/util';
    import { Button } from '$lib/components/ui/button';
    import { Slider } from '$lib/components/ui/slider';
    import { Label } from '$lib/components/ui/label';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as Empty from '$lib/components/ui/empty';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import { taskTray } from '$lib/stores/tasks.svelte';

    let limit = $state(260);
    let nodes = $state<PathTreeNode[]>([]);
    let loading = $state(false);
    let deleteTarget = $state<PathTreeNode | null>(null);
    const selection = new TreeSelection();

    async function reload() {
        if (app.activeWorkspaceId == null) return;
        loading = true;
        try {
            nodes = await api.pathfixTree(app.activeWorkspaceId, limit);
        } catch (err) {
            taskTray.notify('Scan failed', 'error', String(err));
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
        const byId = new Map(nodes.map(n => [n.id, n]));
        return { byParent, byId };
    });

    function childrenOf(parentId: number | null): PathTreeNode[] {
        return index.byParent.get(parentId) ?? [];
    }

    /**
     * A node's path within the end-state tree, root-first -- the same path
     * pathfix.rs measures against the 260-char limit. `name` already reflects
     * any pending virtual rename.
     */
    const pathOf = (id: number) => pathSegments(index.byId, id);

    /** Ids in `ids` that lie under some other id also in `ids` -- moving the
     *  ancestor already carries them along, so moving them again to the
     *  same target would misplace them out of the just-moved folder. Walks
     *  each id's own parent chain rather than a per-pair descendant
     *  closure, to stay O(k*depth) instead of O(k^2). */
    function topLevelOf(ids: number[]): number[] {
        const idsSet = new Set(ids);
        const byId = index.byId;
        return ids.filter(id => {
            let pid = byId.get(id)?.parent_id ?? null;
            while (pid != null) {
                if (idsSet.has(pid)) return false;
                pid = byId.get(pid)?.parent_id ?? null;
            }
            return true;
        });
    }

    /** Combined descendant closure (including the roots) of every id in
     *  `roots`, computed in one pass via `childrenOf` rather than one
     *  descendantsOf() call per root. */
    function descendantsOfAll(roots: number[]): Set<number> {
        const closure = new Set<number>(roots);
        const stack = [...roots];
        while (stack.length) {
            const id = stack.pop()!;
            for (const child of childrenOf(id)) {
                if (!closure.has(child.id)) {
                    closure.add(child.id);
                    stack.push(child.id);
                }
            }
        }
        return closure;
    }

    async function handleMoveManyWithin(
        ids: number[],
        newParentId: number | null
    ) {
        if (app.activeWorkspaceId == null) return;
        const topLevel = topLevelOf(ids);
        if (
            newParentId != null &&
            descendantsOfAll(topLevel).has(newParentId)
        ) {
            taskTray.notify(
                'Move failed',
                'error',
                "Can't move a folder into itself."
            );
            return;
        }
        const base = Math.max(0, ...childrenOf(newParentId).map(n => n.sort_order)) + 1;
        try {
            for (let i = 0; i < topLevel.length; i++) {
                await api.consolidationMoveNode(topLevel[i], newParentId, base + i);
            }
            await reload();
        } catch (err) {
            taskTray.notify('Move failed', 'error', String(err));
        }
    }

    function handleDrop(parentId: number | null, e: DragEvent) {
        const raw = e.dataTransfer?.getData('application/x-dedup-cons-node');
        if (!raw) return;
        const { ids } = JSON.parse(raw) as { ids: number[] };
        handleMoveManyWithin(ids, parentId);
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
            taskTray.notify('Rename failed', 'error', String(err));
        }
    }

    async function handleRevert(node: PathTreeNode) {
        if (app.activeWorkspaceId == null) return;
        try {
            await api.pathfixRename(app.activeWorkspaceId, node.id, '');
            await reload();
        } catch (err) {
            taskTray.notify('Revert failed', 'error', String(err));
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
            taskTray.notify('Remove failed', 'error', String(err));
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
                icon={loading ? 'ph:spinner-gap-fill' : 'ph:scan-fill'}
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
                        {pathOf}
                        {limit}
                        {selection}
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
