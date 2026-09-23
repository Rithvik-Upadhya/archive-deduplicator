<script lang="ts">
    import { untrack } from 'svelte';
    import type { PathTreeNode } from '$lib/types';
    import { app } from '$lib/stores/app.svelte';
    import { copyPath, type SourceBars } from '$lib/util';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import { consolidationTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import Self from './PathTreeItem.svelte';
    import SourceBarsCell from './SourceBarsCell.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';

    interface Props {
        node: PathTreeNode;
        childrenOf: (parentId: number | null) => PathTreeNode[];
        /** Path of a node within the end-state tree, root-first. */
        pathOf: (id: number) => string[];
        /** Source-colour bars for a row: own source first, then (folders)
         *  every other source nested beneath it. */
        barsOf: (node: PathTreeNode) => SourceBars;
        limit: number;
        selection: TreeSelection;
        onrename: (node: PathTreeNode, newName: string) => void;
        onrevert: (node: PathTreeNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
        ondelete: (node: PathTreeNode) => void;
    }

    let {
        node,
        childrenOf,
        pathOf,
        barsOf,
        limit,
        selection,
        onrename,
        onrevert,
        ondropInto,
        ondelete,
    }: Props = $props();

    const isDir = $derived(node.type === 'directory');
    const copyLabel = $derived(
        isDir ? 'Copy folder path' : 'Copy file path'
    );
    // Where this node came from, in the same shape the Consolidate tree uses.
    // A folder the user created by hand has no source, so it falls back to its
    // own name. This is hover text only: the copy button deliberately yields
    // the end-state path instead, with no device name in it.
    const origin = $derived(
        node.origin_device && node.origin_path
            ? `${node.origin_device}://${node.origin_path}`
            : node.name
    );
    // A renamed row keeps its original name in the tooltip, composed with the
    // origin rather than replacing it, so an edited row shows both. It used to
    // end "-- click to rename", which was wrong: renaming is the pencil button,
    // and clicking the row selects it.
    const nameTitle = $derived(
        node.edited
            ? `${origin}\nOriginally “${node.original_name}”`
            : origin
    );
    const kids = $derived(childrenOf(node.id));
    const bars = $derived(barsOf(node));
    const isLeaf = $derived(kids.length === 0);

    // Applied once, on this node's first-ever encounter, not a reactive
    // re-sync: a branch that only becomes over-limit after the slider moves
    // shouldn't retroactively snap open, and one the user collapsed
    // shouldn't reopen just because this component remounted.
    // `untrack` states that in code, not just in the comment: the initial
    // values are exactly what we want here, so the warning about reading them
    // locally is describing the intent rather than a mistake.
    untrack(() => consolidationTreeExpanded.seedOnce(node.id, node.over_limit));
    const expanded = $derived(consolidationTreeExpanded.has(node.id));
    let editing = $state(false);
    let editValue = $state('');
    let dragOver = $state(false);

    function startRename() {
        editing = true;
        editValue = node.name;
    }

    function saveRename() {
        editing = false;
        onrename(node, editValue);
    }

    function onDragStart(e: DragEvent) {
        if (!e.dataTransfer || editing) return;
        e.dataTransfer.setData(
            'application/x-dedup-cons-node',
            JSON.stringify({ ids: selection.dragIds(node.id) })
        );
        e.dataTransfer.effectAllowed = 'move';
    }

    function onRowClick(e: MouseEvent | KeyboardEvent) {
        if (editing) return;
        selection.click(node.id, e);
    }

    function onDrop(e: DragEvent) {
        e.preventDefault();
        e.stopPropagation();
        dragOver = false;
        // Only directories accept children; dropping on a file re-parents
        // to that file's own parent (i.e. drops as a sibling).
        if (isDir) {
            // Otherwise the moved node lands inside a still-collapsed
            // directory and silently vanishes from view.
            consolidationTreeExpanded.add(node.id);
        }
        ondropInto(isDir ? node.id : node.parent_id, e);
    }
</script>

<div class="text-sm">
    <div
        class="group/row flex items-center gap-1.5 border border-transparent px-1.5 py-0.5 select-none data-[dir=true]:bg-muted/50 data-[over=true]:border-destructive/50 data-[over=true]:bg-destructive/[0.06] data-[drag=true]:border-brand data-[drag=true]:bg-brand/15 data-[selected=true]:bg-accent"
        data-dir={isDir}
        data-over={node.over_limit}
        data-drag={dragOver}
        data-selected={selection.isSelected(node.id)}
        draggable={!editing}
        role="treeitem"
        aria-selected={selection.isSelected(node.id)}
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        use:selectable={{ selection, id: node.id }}
        ondragstart={onDragStart}
        ondragover={e => {
            e.preventDefault();
            e.stopPropagation();
            dragOver = true;
        }}
        ondragleave={e => {
            e.stopPropagation();
            dragOver = false;
        }}
        ondrop={onDrop}
        onclick={onRowClick}
        onkeydown={e => e.key === 'Enter' && onRowClick(e)}>
        <button
            type="button"
            class="inline-flex w-3 shrink-0 justify-center text-muted-foreground transition-transform duration-150"
            class:rotate-90={expanded}
            class:invisible={!isDir}
            aria-label="Toggle"
            onclick={e => {
                e.stopPropagation();
                consolidationTreeExpanded.toggle(node.id);
            }}>
            <Icon icon="ph:caret-right-bold" />
        </button>
        <Icon
            icon={isDir ? 'ph:folder-fill' : 'ph:file-fill'}
            class="shrink-0 {node.over_limit
                ? 'text-destructive'
                : 'text-muted-foreground'}" />

        {#if editing}
            <!-- svelte-ignore a11y_autofocus -->
            <Input
                autofocus
                class="h-6 flex-1 text-sm"
                bind:value={editValue}
                onblur={saveRename}
                onkeydown={e => {
                    if (e.key === 'Enter') saveRename();
                    if (e.key === 'Escape') editing = false;
                }} />
        {:else}
            <span
                class="flex-1 truncate {node.over_limit
                    ? 'text-destructive'
                    : ''}"
                title={nameTitle}>
                {node.name}
            </span>
        {/if}

        {#if isLeaf}
            <Badge
                variant={node.path_length > limit ? 'destructive' : 'secondary'}
                class="ms-auto shrink-0 px-1.5 py-0 text-[0.7rem]">
                {node.path_length} chars
            </Badge>
        {/if}

        {#if node.edited}
            <Button
                variant="ghost"
                size="icon"
                class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
                title="Revert to original name"
                onclick={e => {
                    e.stopPropagation();
                    onrevert(node);
                }}>
                <Icon icon="ph:arrow-counter-clockwise-bold" />
                <span class="sr-only">Revert</span>
            </Button>
        {/if}
        <SourceBarsCell {bars} />

        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover/row:opacity-100 hover:text-foreground"
            title={copyLabel}
            aria-label={copyLabel}
            onclick={e => {
                e.stopPropagation();
                // Deliberately not `origin`: hovering shows where this came
                // from, but what you copy is the end-state path, free of any
                // device name.
                copyPath(pathOf(node.id), app.pathSep);
            }}>
            <Icon icon="ph:copy-fill" />
        </Button>
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
            aria-label="Rename"
            onclick={e => {
                e.stopPropagation();
                startRename();
            }}>
            <Icon icon="ph:pencil-simple-fill" />
        </Button>
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-destructive"
            aria-label="Remove"
            onclick={e => {
                e.stopPropagation();
                ondelete(node);
            }}>
            <Icon icon="ph:x-bold" />
        </Button>
    </div>

    {#if isDir && expanded}
        <div class="ms-3 border-l border-border ps-1">
            {#each kids as child (child.id)}
                <Self
                    node={child}
                    {childrenOf}
                    {pathOf}
                    {barsOf}
                    {limit}
                    {selection}
                    {onrename}
                    {onrevert}
                    {ondropInto}
                    {ondelete} />
            {/each}
        </div>
    {/if}
</div>
