<script lang="ts">
    import type { ConsolidationNode } from '$lib/types';
    import Self from './ConsolidationNodeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';

    interface Props {
        node: ConsolidationNode;
        childrenOf: (parentId: number | null) => ConsolidationNode[];
        ondelete: (node: ConsolidationNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
    }

    let { node, childrenOf, ondelete, ondropInto }: Props = $props();

    const isDir = $derived(node.type === 'directory');
    let dragOver = $state(false);

    function onDrop(e: DragEvent) {
        e.preventDefault();
        e.stopPropagation();
        dragOver = false;
        // Only directories accept children.
        ondropInto(isDir ? node.id : node.parent_id, e);
    }
</script>

<div>
    <div
        class="flex items-center gap-2 rounded-sm border border-transparent px-1.5 py-1 text-sm data-[dir=true]:bg-muted/50 data-[drag=true]:border-brand data-[drag=true]:bg-brand/15"
        data-dir={isDir}
        data-drag={dragOver}
        role="treeitem"
        aria-selected="false"
        tabindex="0"
        ondragover={e => {
            if (isDir) {
                e.preventDefault();
                dragOver = true;
            }
        }}
        ondragleave={() => (dragOver = false)}
        ondrop={onDrop}>
        <Icon
            icon={isDir ? 'ph:folder-fill' : 'ph:file-fill'}
            class="shrink-0 text-muted-foreground" />
        <span class="flex-1 truncate" title={node.name}>{node.name}</span>

        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-destructive"
            aria-label="Remove"
            onclick={() => ondelete(node)}>
            <Icon icon="ph:x-bold" />
        </Button>
    </div>

    {#if isDir}
        <div class="ms-3 border-l border-border ps-1">
            {#each childrenOf(node.id) as child (child.id)}
                <Self
                    node={child}
                    {childrenOf}
                    {ondelete}
                    {ondropInto} />
            {/each}
        </div>
    {/if}
</div>
