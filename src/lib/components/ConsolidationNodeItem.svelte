<script lang="ts">
    import type { ConsolidationNode } from '$lib/types';
    import { formatBytes } from '$lib/util';
    import Self from './ConsolidationNodeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';

    interface Props {
        node: ConsolidationNode;
        childrenOf: (parentId: number | null) => ConsolidationNode[];
        stats: Map<number, { size: number; fileCount: number }>;
        ondelete: (node: ConsolidationNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
        onrename: (node: ConsolidationNode, newName: string) => void;
    }

    let { node, childrenOf, stats, ondelete, ondropInto, onrename }: Props =
        $props();

    const isDir = $derived(node.type === 'directory');
    const kids = $derived(childrenOf(node.id));
    const nodeStats = $derived(stats.get(node.id));
    const origin = $derived(
        node.origin_device && node.origin_path
            ? `${node.origin_device}://${node.origin_path}`
            : node.name
    );

    let expanded = $state(false);
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
            JSON.stringify({ id: node.id })
        );
        e.dataTransfer.effectAllowed = 'move';
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
        class="group/row flex items-center gap-1.5 rounded-sm border border-transparent px-1.5 py-0.5 select-none data-[dir=true]:bg-muted/50 data-[drag=true]:border-brand data-[drag=true]:bg-brand/15"
        data-dir={isDir}
        data-drag={dragOver}
        draggable={!editing}
        role="treeitem"
        aria-selected="false"
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        ondragstart={onDragStart}
        ondragover={e => {
            e.preventDefault();
            dragOver = true;
        }}
        ondragleave={() => (dragOver = false)}
        ondrop={onDrop}>
        <button
            type="button"
            class="inline-flex w-3 shrink-0 justify-center text-muted-foreground transition-transform duration-150"
            class:rotate-90={expanded}
            class:invisible={!isDir}
            aria-label="Toggle"
            onclick={() => (expanded = !expanded)}>
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

        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
            aria-label="Rename"
            onclick={startRename}>
            <Icon icon="ph:pencil-simple-fill" />
        </Button>
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-destructive"
            aria-label="Remove"
            onclick={() => ondelete(node)}>
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
                    {ondelete}
                    {ondropInto}
                    {onrename} />
            {/each}
        </div>
    {/if}
</div>
