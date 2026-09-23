<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type {
        ConsolidationNode,
        NodeType,
        TreeNode,
    } from '$lib/types';
    import {
        consolidationTreeExpanded,
        deviceFilterOn,
        deviceTreeExpanded,
    } from '$lib/stores/treeExpansion.svelte';
    import { TreeSelection } from '$lib/stores/selection.svelte';
    import DeviceTree from './DeviceTree.svelte';
    import ConsolidationNodeItem from './ConsolidationNodeItem.svelte';
    import GroupDialog from './GroupDialog.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import NewFolderDialog from './NewFolderDialog.svelte';
    import CollapseAllButton from './CollapseAllButton.svelte';
    import RemoveNodesDialog from './RemoveNodesDialog.svelte';
    import * as Empty from '$lib/components/ui/empty';
    import * as Resizable from '$lib/components/ui/resizable/index.js';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import { AUTO_EXPAND_LIMIT, search } from '$lib/stores/search.svelte';
    import {
        formatBytes,
        countBelow,
        pathSegments,
        strikeToggle,
        sourceBarsFor,
        relocationsFor,
        compareTreeRows,
    } from '$lib/util';

    let consolidationId = $state<number | null>(null);
    let nodes = $state<ConsolidationNode[]>([]);
    let rootDragOver = $state(false);

    // Two independent selections: one for the source-device panel, one for
    // the consolidation tree -- separate id spaces and separate drag
    // "domains" (drag from source into consolidation, or move within it).
    const sourceSelection = new TreeSelection<{
        name: string;
        type: NodeType;
        source_id: number;
        cross_dup: boolean;
    }>();
    const consSelection = new TreeSelection();

    let showNewFolderDialog = $state(false);
    /** Folder the new one goes inside, or null for the root. */
    let newFolderParent = $state<ConsolidationNode | null>(null);
    /** Roots awaiting the remove confirmation, or null when it is closed. */
    let deleteIds = $state<number[] | null>(null);

    async function load() {
        if (app.activeWorkspaceId == null) return;
        try {
            const [cid, cnodes] = await api.consolidationGet(
                app.activeWorkspaceId
            );
            consolidationId = cid;
            nodes = cnodes;
        } catch (err) {
            taskTray.notify(
                'Failed to load consolidation tree',
                'error',
                String(err)
            );
        }
    }

    // Reload whenever the active workspace changes, or whenever the source
    // trees change underneath us. The latter matters because deleting a source
    // also deletes its files from the consolidation tree in the same
    // transaction; without this the in-memory `nodes` array would keep showing
    // them until the next workspace switch.
    let loadedFor = $state<number | null>(null);
    let loadedTreeVersion = $state<number | null>(null);
    $effect(() => {
        if (
            app.activeWorkspaceId != null &&
            (app.activeWorkspaceId !== loadedFor ||
                app.treeVersion !== loadedTreeVersion)
        ) {
            loadedFor = app.activeWorkspaceId;
            loadedTreeVersion = app.treeVersion;
            load();
        }
    });

    // Indexed view over the flat node list: parent -> children (folders first,
    // then by name -- `compareTreeRows`, shared with Fix Paths), plus
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
            bucket.sort(compareTreeRows);
        }

        // Rows struck themselves or through an ancestor. `struck` is stored
        // only where it was applied and inherited here, top-down.
        const struck = new Set<number>();
        const stack: [number | null, boolean][] = [[null, false]];
        while (stack.length) {
            const [pid, inherited] = stack.pop()!;
            for (const n of byParent.get(pid) ?? []) {
                const on = inherited || n.struck;
                if (on) struck.add(n.id);
                stack.push([n.id, on]);
            }
        }

        // Rows that are done *and* have every descendant done -- the only
        // rows shown green. A tick on a folder whose contents are still
        // outstanding keeps its tick icon but is not "finished". Post-order:
        // a node is decided once all its children are.
        const fullyDone = new Set<number>();
        const order: ConsolidationNode[] = [];
        const walk = [...(byParent.get(null) ?? [])];
        while (walk.length) {
            const n = walk.pop()!;
            order.push(n);
            walk.push(...(byParent.get(n.id) ?? []));
        }
        for (let i = order.length - 1; i >= 0; i--) {
            const n = order[i];
            if (
                n.done &&
                (byParent.get(n.id) ?? []).every(c => fullyDone.has(c.id))
            )
                fullyDone.add(n.id);
        }

        const stats = new Map<number, { size: number; fileCount: number }>();
        const byId = new Map(nodes.map(n => [n.id, n]));
        // Counts count names, sizes count bytes actually held -- the same
        // split the source tree uses. A hardlink alias is a real name someone
        // must recreate, so it counts; its bytes are the canonical's and were
        // already added, so adding them again would inflate every ancestor
        // (node_modules read 7.6 MB where the content occupies 1.9 MB).
        // Symlinks arrive here as type 'file' via normalize_type and have no
        // content bytes of their own, so `size` is null and contributes 0.
        //
        // A struck file is to be deleted, not recreated, so it is not part of
        // the end state: it counts only toward ancestors that are struck too
        // (a struck folder still shows what it holds), never toward a live
        // folder or the total. Struck-ness inherits downward, so once the
        // walk up reaches a live ancestor every ancestor above is live too.
        const total = { size: 0, fileCount: 0 };
        for (const n of nodes) {
            if (n.type !== 'file') continue;
            const size = n.is_alias ? 0 : (n.size ?? 0);
            const fileStruck = struck.has(n.id);
            if (!fileStruck) {
                total.size += size;
                total.fileCount += 1;
            }
            let pid = n.parent_id;
            while (pid != null) {
                if (fileStruck && !struck.has(pid)) break;
                const cur = stats.get(pid) ?? { size: 0, fileCount: 0 };
                cur.size += size;
                cur.fileCount += 1;
                stats.set(pid, cur);
                pid = byId.get(pid)?.parent_id ?? null;
            }
        }

        // After-name indicators: renamed items and to-delete items anywhere
        // beneath a row, so they stay findable with folders collapsed.
        const renamedBelow = countBelow(nodes, n => n.original_name != null);
        const struckBelow = countBelow(nodes, n => struck.has(n.id));

        return {
            byParent,
            stats,
            byId,
            total,
            struck,
            fullyDone,
            renamedBelow,
            struckBelow,
        };
    });

    const barsOf = $derived(sourceBarsFor(nodes, app.sources));
    const relocOf = $derived(relocationsFor(nodes, app.sources));

    /** A node's path within the consolidated tree, root-first. */
    const pathOf = (id: number) => pathSegments(index.byId, id);

    function childrenOf(parentId: number | null): ConsolidationNode[] {
        return index.byParent.get(parentId) ?? [];
    }

    // The top-bar search, applied to the in-memory end-state tree: matches on
    // the displayed (possibly renamed) name, plus every ancestor of one. Null
    // when not searching.
    const searchHits = $derived.by(() => {
        if (!search.active) return null;
        const matched = nodes.filter(n => search.matches(n.name));
        const ancestors = new Set<number>();
        for (const n of matched) {
            let pid = n.parent_id;
            while (pid != null && !ancestors.has(pid)) {
                ancestors.add(pid);
                pid = index.byId.get(pid)?.parent_id ?? null;
            }
        }
        const visible = new Set(ancestors);
        for (const n of matched) visible.add(n.id);
        return { matched: matched.length, ancestors, visible };
    });

    /** `childrenOf` minus what the search hides. Rendering only: drops,
     *  sort orders and totals keep using the unfiltered tree. */
    function visibleChildrenOf(parentId: number | null): ConsolidationNode[] {
        const kids = childrenOf(parentId);
        return searchHits ? kids.filter(n => searchHits.visible.has(n.id)) : kids;
    }

    // Open the folders leading to the matches once per search, so later
    // collapses stick. Same cap as the device trees.
    let seededFor = -1;
    $effect(() => {
        const gen = search.generation;
        if (gen === seededFor || !searchHits) return;
        seededFor = gen;
        if (searchHits.matched > AUTO_EXPAND_LIMIT) return;
        for (const id of searchHits.ancestors) search.consExpanded.add(id);
    });

    // Selections the search might hide must not ride along into a drag,
    // strike or delete, so a search starting or ending clears them.
    let clearedFor = search.generation;
    $effect(() => {
        const gen = search.generation;
        if (gen === clearedFor) return;
        clearedFor = gen;
        sourceSelection.clear();
        consSelection.clear();
    });

    async function handleDropFromSource(parentId: number | null, e: DragEvent) {
        const raw = e.dataTransfer?.getData('application/x-dedup-node');
        if (!raw || consolidationId == null || app.activeWorkspaceId == null)
            return;
        const { items } = JSON.parse(raw) as {
            items: {
                node_id: number;
                name: string;
                type: NodeType;
                source_id: number;
                cross_dup: boolean;
            }[];
        };
        // Sequential, not Promise.all: each call mutates the same
        // consolidation tree and appends to `nodes`, so keeping them
        // ordered avoids interleaved/racy array updates.
        let allOk = true;
        for (const item of items) {
            // Honour each source's "exclusive to this device" funnel, keyed
            // on the item's own source (a multi-select can span devices with
            // different funnel states).
            const filtered = deviceFilterOn.has(item.source_id);
            // A hidden row that slipped into the payload (e.g. selected
            // before the funnel was toggled on) -- skip it: the user never
            // saw it, so never chose to drag it. Hidden rows *inside* a
            // dragged directory come across struck instead (the backend
            // marks them), so the end state records what the funnel excluded.
            if (filtered && item.cross_dup) continue;
            try {
                if (item.type === 'directory') {
                    const created = await api.consolidationAddSourceSubtree({
                        consolidationId,
                        parentId,
                        sourceNodeId: item.node_id,
                        filterCrossDup: filtered,
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
                taskTray.notify('Add failed', 'error', String(err));
                allOk = false;
            }
        }
        // Only clear on full success -- on a partial failure the source
        // rows weren't touched, so leaving them selected lets the user see
        // what's left and retry rather than losing track of it.
        if (allOk) sourceSelection.clear();
    }

    /** Combined descendant closure (including the roots) of every id in
     *  `roots`, computed in one pass via `childrenOf` rather than one
     *  closure per root. */
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

    // "Locate duplicates" on a device-tree row: show the node's match group in
    // a dialog, rather than leaving for the Deduplicate view.
    let groupOpen = $state(false);
    let groupNode = $state<TreeNode | null>(null);

    function onlocate(node: TreeNode) {
        groupNode = node;
        groupOpen = true;
    }

    async function handleMoveManyWithin(
        ids: number[],
        newParentId: number | null
    ) {
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
        const base =
            Math.max(0, ...childrenOf(newParentId).map(n => n.sort_order)) + 1;
        for (let i = 0; i < ids.length; i++) {
            const nodeId = ids[i];
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
                taskTray.notify('Move failed', 'error', String(err));
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

    /** Renames go through `pathfixRename`, the same path Fix Paths uses, so
     *  every rename records its original and can be reset from either view.
     *  `''` resets. */
    async function applyRename(node: ConsolidationNode, name: string) {
        if (app.activeWorkspaceId == null) return;
        try {
            const r = await api.pathfixRename(
                app.activeWorkspaceId,
                node.id,
                name
            );
            nodes = nodes.map(n =>
                n.id === node.id
                    ? { ...n, name: r.name, original_name: r.original_name }
                    : n
            );
        } catch (err) {
            taskTray.notify('Rename failed', 'error', String(err));
        }
    }

    function handleRename(node: ConsolidationNode, newName: string) {
        const name = newName.trim();
        if (!name || name === node.name) return;
        applyRename(node, name);
    }

    function handleReset(node: ConsolidationNode) {
        applyRename(node, '');
    }

    // --- Archivist marks & removal, acting on the consolidated selection ---
    //
    // Everything starts from `consSelection.roots()`: a selected folder
    // selects its contents too, and roots never nest, so no row is reached
    // twice. `effective` widens the roots back out to every selected row.

    const hasSelection = $derived(consSelection.selected.size > 0);

    function selectionScope() {
        const roots = consSelection.roots();
        return { roots, effective: [...descendantsOfAll(roots)] };
    }

    /** Toggle: if every selected row is already done, clear them all. */
    async function toggleDone() {
        const { effective } = selectionScope();
        if (effective.length === 0) return;
        const value = !effective.every(id => index.byId.get(id)?.done);
        try {
            await api.consolidationSetDone(effective, value);
            const hit = new Set(effective);
            nodes = nodes.map(n => (hit.has(n.id) ? { ...n, done: value } : n));
        } catch (err) {
            taskTray.notify('Update failed', 'error', String(err));
        }
    }

    /** Toggle: strike the roots (their contents inherit), or -- if every
     *  root is already struck -- clear the flag on every selected row, so a
     *  descendant struck on its own does not survive the un-strike. A root
     *  struck only through an unselected ancestor stays struck. */
    async function toggleStruck() {
        const { roots, effective } = selectionScope();
        if (roots.length === 0) return;
        const { ids, value } = strikeToggle(roots, effective, id =>
            index.struck.has(id)
        );
        try {
            await api.consolidationSetStruck(ids, value);
            const hit = new Set(ids);
            nodes = nodes.map(n =>
                hit.has(n.id) ? { ...n, struck: value } : n
            );
        } catch (err) {
            taskTray.notify('Update failed', 'error', String(err));
        }
    }

    function openDeleteDialog() {
        const roots = consSelection.roots();
        if (roots.length > 0) deleteIds = roots;
    }

    async function confirmDelete(roots: number[]) {
        deleteIds = null;
        if (roots.length === 0) return;
        const toRemove = descendantsOfAll(roots);
        try {
            await api.consolidationDeleteNodes(roots);
            nodes = nodes.filter(n => !toRemove.has(n.id));
            consSelection.clear();
        } catch (err) {
            taskTray.notify('Remove failed', 'error', String(err));
        }
    }

    /** At the root from the toolbar, inside `parent` from a row's menu. */
    function openNewFolderDialog(parent: ConsolidationNode | null = null) {
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        newFolderParent = parent;
        showNewFolderDialog = true;
    }

    async function createFolder(name: string) {
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        const parent = newFolderParent;
        try {
            const created = await api.consolidationAddNode({
                consolidationId,
                parentId: parent?.id ?? null,
                name,
                nodeType: 'directory',
                sourceNodeId: null,
            });
            nodes = [...nodes, created];
            // Otherwise the new folder lands inside a collapsed parent and
            // seems not to have been made.
            if (parent)
                (search.active
                    ? search.consExpanded
                    : consolidationTreeExpanded
                ).add(parent.id);
        } catch (err) {
            taskTray.notify('Create failed', 'error', String(err));
        }
    }
</script>

<Resizable.PaneGroup
    direction="horizontal"
    class="min-h-0 grow overflow-hidden">
    <!-- Source devices -->
    <Resizable.Pane>
        <section class="flex h-full min-h-0 min-w-0 flex-col overflow-hidden pe-1">
            <div class="mb-2 flex shrink-0 items-center justify-between gap-2">
                <h2 class="section-label">
                    Source devices — drag files &amp; folders →
                </h2>
                <CollapseAllButton
                    set={search.active
                        ? search.deviceExpanded
                        : deviceTreeExpanded} />
            </div>
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
                <div
                    data-device-list
                    class="flex flex-col overflow-y-auto overflow-x-hidden">
                    {#each app.visibleSources as s (s.id)}
                        <DeviceTree
                            source={s}
                            draggable
                            selection={sourceSelection}
                            {onlocate} />
                    {/each}
                </div>
            {/if}
        </section>
    </Resizable.Pane>
    <Resizable.Handle class="mx-3" />
    <!-- Consolidated target tree -->
    <Resizable.Pane>
        <section class="flex h-full min-h-0 min-w-0 flex-col overflow-hidden">
            <div class="mb-2 flex items-center justify-between gap-2">
                <div class="flex min-w-0 items-baseline gap-2">
                    <h2 class="section-label">Consolidated tree</h2>
                    <span class="truncate text-xs text-muted-foreground">
                        {index.total.fileCount.toLocaleString()}
                        {index.total.fileCount === 1 ? 'file' : 'files'} ·
                        {formatBytes(index.total.size)}
                    </span>
                </div>
                <div class="flex shrink-0 items-center gap-1.5">
                    {#if hasSelection}
                        <Button
                            variant="outline"
                            size="sm"
                            title="Remove selected items from the consolidated tree"
                            aria-label="Remove selected"
                            onclick={openDeleteDialog}>
                            <Icon icon="ph:trash-fill" />
                        </Button>
                        <Button
                            variant="outline"
                            size="sm"
                            title="Mark selected items to be deleted (toggle). An item struck through a folder above the selection stays struck."
                            aria-label="Toggle to-delete mark"
                            onclick={toggleStruck}>
                            <Icon icon="ph:text-strikethrough-bold" />
                        </Button>
                        <Button
                            variant="outline"
                            size="sm"
                            title="Mark selected items as done (toggle)"
                            aria-label="Toggle done mark"
                            onclick={toggleDone}>
                            <Icon icon="ph:check-bold" />
                        </Button>
                    {/if}
                    <Button
                        variant="outline"
                        size="sm"
                        onclick={() => openNewFolderDialog()}>
                        <Icon icon="ph:folder-plus-fill" />
                        <span>Folder</span>
                    </Button>
                    <CollapseAllButton
                        set={search.active
                            ? search.consExpanded
                            : consolidationTreeExpanded} />
                </div>
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
                {:else if visibleChildrenOf(null).length === 0}
                    <p class="p-4 text-center text-sm text-muted-foreground">
                        No matches in the consolidated tree.
                    </p>
                {:else}
                    {#each visibleChildrenOf(null) as node (node.id)}
                        <ConsolidationNodeItem
                            {node}
                            childrenOf={visibleChildrenOf}
                            stats={index.stats}
                            {barsOf}
                            {relocOf}
                            {pathOf}
                            selection={consSelection}
                            isStruck={id => index.struck.has(id)}
                            isFullyDone={id => index.fullyDone.has(id)}
                            renamedBelowOf={id => index.renamedBelow.get(id) ?? 0}
                            struckBelowOf={id => index.struckBelow.get(id) ?? 0}
                            onreset={handleReset}
                            onnewfolder={openNewFolderDialog}
                            ondropInto={handleDrop}
                            onrename={handleRename} />
                    {/each}
                {/if}
            </div>
        </section>
    </Resizable.Pane>
</Resizable.PaneGroup>

<GroupDialog bind:open={groupOpen} node={groupNode} />

<NewFolderDialog
    bind:open={showNewFolderDialog}
    parentName={newFolderParent?.name ?? null}
    oncreate={createFolder} />

<RemoveNodesDialog
    ids={deleteIds}
    nodeOf={id => index.byId.get(id)}
    note="To keep a note that something must be deleted on disk, strike it through instead."
    onconfirm={confirmDelete}
    oncancel={() => (deleteIds = null)} />
