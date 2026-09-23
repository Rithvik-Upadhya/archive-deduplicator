<script lang="ts">
    import { untrack } from 'svelte';
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { PathTreeNode } from '$lib/types';
    import { TreeSelection } from '$lib/stores/selection.svelte';
    import { consolidationTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import PathTreeItem from './PathTreeItem.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import {
        countBelow,
        pathSegments,
        sourceBarsFor,
        relocationsFor,
        compareTreeRows,
        strikeToggle,
    } from '$lib/util';
    import { Button } from '$lib/components/ui/button';
    import { Label } from '$lib/components/ui/label';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as Empty from '$lib/components/ui/empty';
    import NewFolderDialog from './NewFolderDialog.svelte';
    import NumberField from './NumberField.svelte';
    import CollapseAllButton from './CollapseAllButton.svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';

    /** The limit in the field -- applied only by Rescan. */
    let limit = $state(260);
    /** The limit the tree on screen was measured at. */
    let scannedLimit = $state(260);
    let nodes = $state<PathTreeNode[]>([]);
    let loading = $state(false);
    let showNewFolderDialog = $state(false);
    /** Folder the new one goes inside, or null for the root. */
    let newFolderParent = $state<PathTreeNode | null>(null);
    const selection = new TreeSelection();

    /**
     * Re-measure the tree. Every mutation (rename, move, strike, drop, new
     * folder) calls this bare, at the *scanned* limit, so an edited row drops
     * out of the red at once while an unrun edit to the field stays unrun --
     * only Rescan passes the field's value.
     */
    async function reload(l = scannedLimit) {
        if (app.activeWorkspaceId == null) return;
        loading = true;
        try {
            nodes = await api.pathfixTree(app.activeWorkspaceId, l);
            scannedLimit = l;
        } catch (err) {
            taskTray.notify('Scan failed', 'error', String(err));
        } finally {
            loading = false;
        }
    }

    // Load on the first visit and on a workspace switch -- never on a limit
    // edit, which waits for Rescan. Plain (non-reactive) on purpose: writing
    // it here must not re-trigger this effect.
    let loadedFor: number | null = null;
    $effect(() => {
        const ws = app.activeWorkspaceId;
        if (ws != null && ws !== loadedFor) {
            loadedFor = ws;
            untrack(() => reload());
        }
    });

    // Indexed view over the flat node list: parent -> children, folders first
    // then by name -- `compareTreeRows`, the same order the Consolidate tree
    // uses. The backend's own `sort_order` walk is not the display order.
    const index = $derived.by(() => {
        const byParent = new Map<number | null, PathTreeNode[]>();
        for (const n of nodes) {
            const bucket = byParent.get(n.parent_id) ?? [];
            bucket.push(n);
            byParent.set(n.parent_id, bucket);
        }
        for (const bucket of byParent.values()) bucket.sort(compareTreeRows);
        const byId = new Map(nodes.map(n => [n.id, n]));
        // Renamed items anywhere beneath a row, for its after-name mark.
        const renamedBelow = countBelow(nodes, n => n.edited);
        // Struck rows are shown but not part of the end state; their
        // unstruck ancestors carry an after-name mark instead.
        const struckBelow = countBelow(nodes, n => n.struck);
        return { byParent, byId, renamedBelow, struckBelow };
    });

    const barsOf = $derived(sourceBarsFor(nodes, app.sources));
    const relocOf = $derived(relocationsFor(nodes, app.sources));

    function childrenOf(parentId: number | null): PathTreeNode[] {
        return index.byParent.get(parentId) ?? [];
    }

    /**
     * A node's path within the end-state tree, root-first -- the same path
     * pathfix.rs measures against the scanned limit. `name` already reflects
     * any pending virtual rename.
     */
    const pathOf = (id: number) => pathSegments(index.byId, id);

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
        // `ids` come from `selection.dragIds`, i.e. the selection's roots, so
        // no id lies beneath another and each node is moved exactly once.
        if (
            newParentId != null &&
            descendantsOfAll(ids).has(newParentId)
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
            for (let i = 0; i < ids.length; i++) {
                await api.consolidationMoveNode(ids[i], newParentId, base + i);
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

    const hasSelection = $derived(selection.selected.size > 0);

    /** Same toggle as the Consolidate toolbar (`strikeToggle`). Reloads
     *  rather than patching locally: striking changes what is measured, and
     *  `struck` here is the backend's inherited value. */
    async function toggleStruck() {
        const roots = selection.roots();
        if (roots.length === 0) return;
        const effective = [...descendantsOfAll(roots)];
        const { ids, value } = strikeToggle(
            roots,
            effective,
            id => index.byId.get(id)?.struck ?? false
        );
        try {
            await api.consolidationSetStruck(ids, value);
            await reload();
        } catch (err) {
            taskTray.notify('Update failed', 'error', String(err));
        }
    }

    /** At the root from the toolbar, inside `parent` from a row's menu. */
    function openNewFolderDialog(parent: PathTreeNode | null = null) {
        newFolderParent = parent;
        showNewFolderDialog = true;
    }

    async function createFolder(name: string) {
        if (app.activeWorkspaceId == null) return;
        const parent = newFolderParent;
        try {
            // Also creates the workspace's consolidation if none exists yet,
            // so a first folder can be made from this view.
            const [consolidationId] = await api.consolidationGet(
                app.activeWorkspaceId
            );
            await api.consolidationAddNode({
                consolidationId,
                parentId: parent?.id ?? null,
                name,
                nodeType: 'directory',
                sourceNodeId: null,
            });
            // Otherwise the new folder lands inside a collapsed parent and
            // seems not to have been made.
            if (parent) consolidationTreeExpanded.add(parent.id);
            await reload();
        } catch (err) {
            taskTray.notify('Create failed', 'error', String(err));
        }
    }

    /** Whether a row has a child that is still part of the end state. */
    const hasLiveKids = (id: number) => childrenOf(id).some(c => !c.struck);

    // Measured leaves only, mirroring pathfix.rs: a live row with no live
    // children. Struck rows are never measured.
    const overCount = $derived(
        nodes.filter(
            n =>
                !n.struck && !hasLiveKids(n.id) && n.path_length > scannedLimit
        ).length
    );
</script>

<div class="flex min-h-0 grow flex-col gap-3 overflow-hidden">
    <div class="flex flex-wrap items-end gap-6">
        <div class="flex flex-col gap-1.5">
            <Label for="limit" class="text-xs text-muted-foreground">
                Path length limit
            </Label>
            <NumberField
                id="limit"
                value={limit}
                suffix="chars"
                min={1}
                max={32767}
                oncommit={v => (limit = v)} />
        </div>
        <div class="flex items-baseline gap-2 text-sm text-muted-foreground">
            <span
                class="text-2xl font-bold"
                class:text-destructive={overCount > 0}
                class:text-ok={overCount === 0}>{overCount}</span>
            <span>branches over limit</span>
        </div>
        <!-- Only offered once the field differs from what is on screen (and
             while a scan is running, so its progress stays visible). -->
        {#if limit !== scannedLimit || loading}
            <Button
                variant="outline"
                disabled={loading}
                onclick={() => reload(limit)}>
                <Icon
                    icon={loading ? 'ph:spinner-gap-fill' : 'ph:scan-fill'}
                    class={loading ? 'animate-spin' : ''} />
                <span>{loading ? 'Scanning…' : 'Rescan'}</span>
            </Button>
        {/if}
        <div class="ms-auto flex shrink-0 items-center gap-1.5">
            {#if hasSelection}
                <Button
                    variant="outline"
                    size="sm"
                    title="Mark selected items to be deleted (toggle). Struck items are not measured. An item struck through a folder above the selection stays struck."
                    aria-label="Toggle to-delete mark"
                    onclick={toggleStruck}>
                    <Icon icon="ph:text-strikethrough-bold" />
                </Button>
            {/if}
            <Button
                variant="outline"
                size="sm"
                onclick={() => openNewFolderDialog()}>
                <Icon icon="ph:folder-plus-fill" />
                <span>Folder</span>
            </Button>
            <!-- No search set here: search is hidden in Fix Paths. -->
            <CollapseAllButton set={consolidationTreeExpanded} />
        </div>
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
                        {barsOf}
                        {relocOf}
                        limit={scannedLimit}
                        {selection}
                        onrename={handleRename}
                        renamedBelowOf={id => index.renamedBelow.get(id) ?? 0}
                        struckBelowOf={id => index.struckBelow.get(id) ?? 0}
                        {hasLiveKids}
                        onrevert={handleRevert}
                        onnewfolder={openNewFolderDialog}
                        ondropInto={handleDrop} />
                {/each}
            </div>
        </ScrollArea>
    {/if}
</div>

<NewFolderDialog
    bind:open={showNewFolderDialog}
    parentName={newFolderParent?.name ?? null}
    oncreate={createFolder} />
