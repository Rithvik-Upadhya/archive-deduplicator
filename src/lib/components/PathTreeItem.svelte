<script lang="ts">
    import type { PathTreeNode } from '$lib/types';
    import Self from './PathTreeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';

    interface Props {
        node: PathTreeNode;
        childrenOf: (parentId: number | null) => PathTreeNode[];
        limit: number;
        onrename: (node: PathTreeNode, newName: string) => void;
        onrevert: (node: PathTreeNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
        ondelete: (node: PathTreeNode) => void;
    }

    let {
        node,
        childrenOf,
        limit,
        onrename,
        onrevert,
        ondropInto,
        ondelete,
    }: Props = $props();

    const isDir = $derived(node.type === 'directory');
    const kids = $derived(childrenOf(node.id));
    const isLeaf = $derived(kids.length === 0);

    // One-time initialization, not a reactive re-sync: a branch that only
    // becomes over-limit after the slider moves shouldn't retroactively
    // snap open, and one the user collapsed shouldn't reopen.
    let expanded = $state(node.over_limit);
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
            JSON.stringify({ id: node.id })
        );
        e.dataTransfer.effectAllowed = 'move';
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
            expanded = true;
        }
        ondropInto(isDir ? node.id : node.parent_id, e);
    }
</script>

<div class="text-sm">
    <div
        class="group/row flex items-center gap-1.5 rounded-sm border border-transparent px-1.5 py-0.5 select-none data-[dir=true]:bg-muted/50 data-[over=true]:border-destructive/50 data-[over=true]:bg-destructive/[0.06] data-[drag=true]:border-brand data-[drag=true]:bg-brand/15"
        data-dir={isDir}
        data-over={node.over_limit}
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
                title={node.edited
                    ? `Originally “${node.original_name}” — click to rename`
                    : node.name}>
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
                onclick={() => onrevert(node)}>
                <Icon icon="ph:arrow-counter-clockwise-bold" />
                <span class="sr-only">Revert</span>
            </Button>
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
                    {limit}
                    {onrename}
                    {onrevert}
                    {ondropInto}
                    {ondelete} />
            {/each}
        </div>
    {/if}
</div>
