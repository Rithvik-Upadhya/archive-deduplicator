<script lang="ts">
    import type { ConsolidationNode } from '$lib/types';
    import { copyPath, formatBytes, type SourceBars } from '$lib/util';
    import { app } from '$lib/stores/app.svelte';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import { consolidationTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import Self from './ConsolidationNodeItem.svelte';
    import SourceBarsCell from './SourceBarsCell.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';

    interface Props {
        node: ConsolidationNode;
        childrenOf: (parentId: number | null) => ConsolidationNode[];
        stats: Map<number, { size: number; fileCount: number }>;
        /** Source-colour bars for a row: own source first, then (folders)
         *  every other source nested beneath it. */
        barsOf: (node: ConsolidationNode) => SourceBars;
        /** Path of a node within the consolidated tree, root-first. */
        pathOf: (id: number) => string[];
        selection: TreeSelection;
        ondelete: (node: ConsolidationNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
        onrename: (node: ConsolidationNode, newName: string) => void;
    }

    let {
        node,
        childrenOf,
        stats,
        barsOf,
        pathOf,
        selection,
        ondelete,
        ondropInto,
        onrename,
    }: Props = $props();

    const isDir = $derived(node.type === 'directory');
    const copyLabel = $derived(
        isDir ? 'Copy folder path' : 'Copy file path'
    );
    const kids = $derived(childrenOf(node.id));
    const nodeStats = $derived(stats.get(node.id));
    const bars = $derived(barsOf(node));
    const origin = $derived(
        node.origin_device && node.origin_path
            ? `${node.origin_device}://${node.origin_path}`
            : node.name
    );

    const expanded = $derived(consolidationTreeExpanded.has(node.id));
    let dragOver = $state(false);
    let editing = $state(false);
    let editValue = $state('');

    function onDrop(e: DragEvent) {
        e.preventDefault();
        e.stopPropagation();
        dragOver = false;
        // Only directories accept children.
        ondropInto(isDir ? node.id : node.parent_id, e);
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

    function startRename() {
        editing = true;
        editValue = node.name;
    }

    function saveRename() {
        editing = false;
        onrename(node, editValue);
    }
</script>

<div class="text-sm">
    <div
        class="group/row flex items-center gap-1.5 border border-transparent px-1.5 py-0.5 select-none data-[dir=true]:bg-muted/50 data-[drag=true]:border-brand data-[drag=true]:bg-brand/15 data-[selected=true]:bg-accent"
        data-dir={isDir}
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
            class="shrink-0 text-muted-foreground" />

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
            <span class="flex-1 truncate" title={origin}>{node.name}</span>
        {/if}

        {#if isDir}
            <span
                class="ms-auto shrink-0 font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
                {nodeStats?.fileCount ?? 0} files · {formatBytes(
                    nodeStats?.size ?? 0
                )}
            </span>
        {:else}
            <span
                class="ms-auto shrink-0 font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
                {formatBytes(node.size ?? 0)}
            </span>
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
                // The walk stops at a consolidation root -- a folder the
                // user made or dropped -- so no source name is included.
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
                    {stats}
                    {barsOf}
                    {pathOf}
                    {selection}
                    {ondelete}
                    {ondropInto}
                    {onrename} />
            {/each}
        </div>
    {/if}
</div>
