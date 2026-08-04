<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { ConsolidationNode, NodeType } from '$lib/types';
    import { TreeSelection } from '$lib/stores/selection.svelte';
    import DeviceTree from './DeviceTree.svelte';
    import ConsolidationNodeItem from './ConsolidationNodeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import * as Dialog from '$lib/components/ui/dialog';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import * as Field from '$lib/components/ui/field';
    import * as Empty from '$lib/components/ui/empty';
    import { toast } from 'svelte-sonner';

    let consolidationId = $state<number | null>(null);
    let nodes = $state<ConsolidationNode[]>([]);
    let rootDragOver = $state(false);

    // Two independent selections: one for the source-device panel, one for
    // the consolidation tree -- separate id spaces and separate drag
    // "domains" (drag from source into consolidation, or move within it).
    const sourceSelection = new TreeSelection<{
        name: string;
        type: NodeType;
    }>();
    const consSelection = new TreeSelection();

    let showNewFolderDialog = $state(false);
    let newFolderName = $state('New Folder');
    let deleteTarget = $state<ConsolidationNode | null>(null);

    async function load() {
        if (app.activeWorkspaceId == null) return;
        const [cid, cnodes] = await api.consolidationGet(app.activeWorkspaceId);
        consolidationId = cid;
        nodes = cnodes;
    }

    // Reload whenever the active workspace changes.
    let loadedFor = $state<number | null>(null);
    $effect(() => {
        if (
            app.activeWorkspaceId != null &&
            app.activeWorkspaceId !== loadedFor
        ) {
            loadedFor = app.activeWorkspaceId;
            load();
        }
    });

    // Indexed view over the flat node list: parent -> sorted children, plus
    // bottom-up folder stats (file count / total size) folded from every
    // descendant file. Recomputed only when `nodes` changes.
    const index = $derived.by(() => {
        const byParent = new Map<number | null, ConsolidationNode[]>();
        for (const n of nodes) {
            const bucket = byParent.get(n.parent_id) ?? [];
            bucket.push(n);
            byParent.set(n.parent_id, bucket);
        }
        for (const bucket of byParent.values()) {
            bucket.sort((a, b) => a.sort_order - b.sort_order);
        }

        const stats = new Map<number, { size: number; fileCount: number }>();
        const byId = new Map(nodes.map(n => [n.id, n]));
        for (const n of nodes) {
            if (n.type !== 'file') continue;
            const size = n.size ?? 0;
            let pid = n.parent_id;
            while (pid != null) {
                const cur = stats.get(pid) ?? { size: 0, fileCount: 0 };
                cur.size += size;
                cur.fileCount += 1;
                stats.set(pid, cur);
                pid = byId.get(pid)?.parent_id ?? null;
            }
        }
        return { byParent, stats };
    });

    function childrenOf(parentId: number | null): ConsolidationNode[] {
        return index.byParent.get(parentId) ?? [];
    }

    /** BFS descendant closure (including the node itself) — shared by
     *  move-cycle prevention and delete. */
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

    async function handleDropFromSource(parentId: number | null, e: DragEvent) {
        const raw = e.dataTransfer?.getData('application/x-dedup-node');
        if (!raw || consolidationId == null || app.activeWorkspaceId == null)
            return;
        const { items } = JSON.parse(raw) as {
            items: { node_id: number; name: string; type: NodeType }[];
        };
        // Sequential, not Promise.all: each call mutates the same
        // consolidation tree and appends to `nodes`, so keeping them
        // ordered avoids interleaved/racy array updates.
        let allOk = true;
        for (const item of items) {
            try {
                if (item.type === 'directory') {
                    const created = await api.consolidationAddSourceSubtree({
                        consolidationId,
                        parentId,
                        sourceNodeId: item.node_id,
                    });
                    nodes = [...nodes, ...created];
                } else {
                    const created = await api.consolidationAddNode({
                        consolidationId,
                        parentId,
                        name: item.name,
                        nodeType: 'file',
                        sourceNodeId: item.node_id,
                    });
                    nodes = [...nodes, created];
                }
            } catch (err) {
                toast.error(String(err));
                allOk = false;
            }
        }
        // Only clear on full success -- on a partial failure the source
        // rows weren't touched, so leaving them selected lets the user see
        // what's left and retry rather than losing track of it.
        if (allOk) sourceSelection.clear();
    }

    /** Ids in `ids` that lie under some other id also in `ids` -- moving the
     *  ancestor already carries them along, so moving them again to the
     *  same target would misplace them out of the just-moved folder. Walks
     *  each id's own parent chain rather than a per-pair descendant
     *  closure, to stay O(k*depth) instead of O(k^2). */
    function topLevelOf(ids: number[]): number[] {
        const idsSet = new Set(ids);
        const byId = new Map(nodes.map(n => [n.id, n]));
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
        const topLevel = topLevelOf(ids);
        if (
            newParentId != null &&
            descendantsOfAll(topLevel).has(newParentId)
        ) {
            toast.error("Can't move a folder into itself.");
            return;
        }
        const base =
            Math.max(0, ...childrenOf(newParentId).map(n => n.sort_order)) + 1;
        for (let i = 0; i < topLevel.length; i++) {
            const nodeId = topLevel[i];
            const newSortOrder = base + i;
            try {
                await api.consolidationMoveNode(
                    nodeId,
                    newParentId,
                    newSortOrder
                );
                nodes = nodes.map(n =>
                    n.id === nodeId
                        ? {
                              ...n,
                              parent_id: newParentId,
                              sort_order: newSortOrder,
                          }
                        : n
                );
            } catch (err) {
                toast.error(String(err));
                break;
            }
        }
    }

    function handleDrop(parentId: number | null, e: DragEvent) {
        const consRaw = e.dataTransfer?.getData(
            'application/x-dedup-cons-node'
        );
        if (consRaw) {
            const { ids } = JSON.parse(consRaw) as { ids: number[] };
            handleMoveManyWithin(ids, parentId);
            return;
        }
        handleDropFromSource(parentId, e);
    }

    async function handleRename(node: ConsolidationNode, newName: string) {
        const name = newName.trim();
        if (!name || name === node.name) return;
        try {
            await api.consolidationRenameNode(node.id, name);
            nodes = nodes.map(n => (n.id === node.id ? { ...n, name } : n));
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function confirmDelete() {
        const target = deleteTarget;
        deleteTarget = null;
        if (!target) return;
        const toRemove = descendantsOf(target.id);
        try {
            await api.consolidationDeleteNode(target.id);
            nodes = nodes.filter(n => !toRemove.has(n.id));
        } catch (err) {
            toast.error(String(err));
        }
    }

    function openNewFolderDialog() {
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        newFolderName = 'New Folder';
        showNewFolderDialog = true;
    }

    async function submitNewFolder(e: SubmitEvent) {
        e.preventDefault();
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        const name = newFolderName.trim();
        if (!name) return;
        showNewFolderDialog = false;
        try {
            const created = await api.consolidationAddNode({
                consolidationId,
                parentId: null,
                name,
                nodeType: 'directory',
                sourceNodeId: null,
            });
            nodes = [...nodes, created];
        } catch (err) {
            toast.error(String(err));
        }
    }
</script>

<div class="grid min-h-0 grow grid-cols-2 gap-4 overflow-hidden">
    <!-- Source devices -->
    <section class="flex min-h-0 min-w-0 flex-col overflow-hidden pe-1">
        <h2 class="section-label mb-2 shrink-0">
            Source devices — drag files &amp; folders →
        </h2>
        {#if app.visibleSources.length === 0}
            <Empty.Root class="border border-dashed">
                <Empty.Header>
                    <Empty.Media variant="icon">
                        <Icon icon="ph:hard-drives-fill" />
                    </Empty.Media>
                    <Empty.Title>No devices</Empty.Title>
                    <Empty.Description>
                        Add devices first in the Deduplicate view.
                    </Empty.Description>
                </Empty.Header>
            </Empty.Root>
        {:else}
            <div class="flex flex-col overflow-y-auto overflow-x-hidden">
                {#each app.visibleSources as s (s.id)}
                    <DeviceTree
                        source={s}
                        draggable
                        selection={sourceSelection} />
                {/each}
            </div>
        {/if}
    </section>

    <!-- Consolidated target tree -->
    <section class="flex min-h-0 min-w-0 flex-col overflow-hidden">
        <div class="mb-2 flex items-center justify-between gap-2">
            <h2 class="section-label">Consolidated tree</h2>
            <Button variant="outline" size="sm" onclick={openNewFolderDialog}>
                <Icon icon="ph:folder-plus-fill" />
                <span>Folder</span>
            </Button>
        </div>
        <div
            class="min-h-32 grow overflow-y-auto rounded-md border-2 border-dashed p-2 transition-colors data-[drag=true]:border-brand data-[drag=true]:bg-brand/10"
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
            {#if childrenOf(null).length === 0}
                <p class="p-4 text-center text-sm text-muted-foreground">
                    Drop files or folders here to build your target tree.
                </p>
            {:else}
                {#each childrenOf(null) as node (node.id)}
                    <ConsolidationNodeItem
                        {node}
                        {childrenOf}
                        stats={index.stats}
                        selection={consSelection}
                        ondelete={n => (deleteTarget = n)}
                        ondropInto={handleDrop}
                        onrename={handleRename} />
                {/each}
            {/if}
        </div>
    </section>
</div>

<Dialog.Root bind:open={showNewFolderDialog}>
    <Dialog.Content class="sm:max-w-sm">
        <form onsubmit={submitNewFolder}>
            <Dialog.Header>
                <Dialog.Title>New folder</Dialog.Title>
                <Dialog.Description>
                    Adds an empty folder at the root of the consolidated tree.
                </Dialog.Description>
            </Dialog.Header>
            <Field.FieldGroup class="py-4">
                <Field.Field>
                    <Field.FieldLabel for="folder-name">Name</Field.FieldLabel>
                    <!-- svelte-ignore a11y_autofocus -->
                    <Input
                        id="folder-name"
                        autofocus
                        bind:value={newFolderName} />
                </Field.Field>
            </Field.FieldGroup>
            <Dialog.Footer>
                <Button
                    type="button"
                    variant="outline"
                    onclick={() => (showNewFolderDialog = false)}
                    >Cancel</Button>
                <Button type="submit" disabled={!newFolderName.trim()}>
                    Create
                </Button>
            </Dialog.Footer>
        </form>
    </Dialog.Content>
</Dialog.Root>

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
