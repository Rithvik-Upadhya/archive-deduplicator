<script lang="ts">
    import type { ConsolidationNode } from '$lib/types';
    import { copyPath, formatBytes, type SourceBars } from '$lib/util';
    import { app } from '$lib/stores/app.svelte';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import { consolidationTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import Self from './ConsolidationNodeItem.svelte';
    import SourceBarsCell from './SourceBarsCell.svelte';
    import NameMarks from './NameMarks.svelte';
    import Icon, { type IconName } from '$lib/components/Icon.svelte';
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
        /** Whether a row is struck itself or through an ancestor. */
        isStruck: (id: number) => boolean;
        /** Whether a row and every descendant are done. */
        isFullyDone: (id: number) => boolean;
        /** Renamed / struck items anywhere beneath a row. */
        renamedBelowOf: (id: number) => number;
        struckBelowOf: (id: number) => number;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
        onrename: (node: ConsolidationNode, newName: string) => void;
        /** Restore a renamed row's original name. */
        onreset: (node: ConsolidationNode) => void;
    }

    let {
        node,
        childrenOf,
        stats,
        barsOf,
        pathOf,
        selection,
        isStruck,
        isFullyDone,
        renamedBelowOf,
        struckBelowOf,
        ondropInto,
        onrename,
        onreset,
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

    const struck = $derived(isStruck(node.id));
    const icon = $derived<IconName>(
        node.done
            ? isDir
                ? 'app:folder-check'
                : 'app:file-check'
            : isDir
              ? 'ph:folder-fill'
              : 'ph:file-fill'
    );
    // Green only once the row *and* everything beneath it is done -- a ticked
    // folder with outstanding contents keeps its tick icon but no tint. Fully
    // done beats struck, so a to-delete row whose deletion is finished turns
    // green (its strikethrough stays); otherwise struck is red.
    const markTone = $derived(
        isFullyDone(node.id) ? 'text-done' : struck ? 'text-struck' : ''
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
        class="group/row flex items-center gap-1.5 border border-transparent px-1.5 py-0.5 select-none data-[drag=true]:border-brand data-[drag=true]:bg-brand/15 data-[selected=true]:bg-muted/50"
        data-drag={dragOver}
        data-selected={selection.isSelected(node.id)}
        draggable={!editing}
        role="treeitem"
        aria-selected={selection.isSelected(node.id)}
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        use:selectable={{ selection, id: node.id, parentId: node.parent_id }}
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
            {icon}
            class="shrink-0 {markTone || 'text-muted-foreground'}" />

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
            <span class="flex min-w-0 flex-1 items-center gap-1">
                <span
                    class="truncate {markTone}"
                    class:line-through={struck}
                    title={origin}>{node.name}</span>
                <NameMarks
                    original={node.original_name}
                    renamedBelow={renamedBelowOf(node.id)}
                    struckBelow={!struck && struckBelowOf(node.id) > 0}
                    onreset={() => onreset(node)} />
            </span>
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
                    {isStruck}
                    {isFullyDone}
                    {renamedBelowOf}
                    {struckBelowOf}
                    {ondropInto}
                    {onrename}
                    {onreset} />
            {/each}
        </div>
    {/if}
</div>
