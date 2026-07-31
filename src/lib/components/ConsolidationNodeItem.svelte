<script lang="ts">
    import type { ConsolidationAction, ConsolidationNode } from '$lib/types';
    import Self from './ConsolidationNodeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import * as Select from '$lib/components/ui/select';

    interface Props {
        node: ConsolidationNode;
        childrenOf: (parentId: number | null) => ConsolidationNode[];
        onaction: (nodeId: number, action: ConsolidationAction) => void;
        ondelete: (node: ConsolidationNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
    }

    let { node, childrenOf, onaction, ondelete, ondropInto }: Props = $props();

    const actions: ConsolidationAction[] = ['keep', 'move', 'copy', 'skip'];
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
        class="flex items-center gap-2 rounded-sm border border-transparent px-1.5 py-1 text-sm data-[dir=true]:bg-muted/50 data-[drag=true]:border-brand data-[drag=true]:bg-brand/15 data-[skip=true]:line-through data-[skip=true]:opacity-50"
        data-dir={isDir}
        data-drag={dragOver}
        data-skip={node.action === 'skip'}
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

        <Select.Root
            type="single"
            value={node.action}
            onValueChange={v =>
                v && onaction(node.id, v as ConsolidationAction)}>
            <Select.Trigger size="sm" class="h-6 w-24 text-xs capitalize">
                {node.action}
            </Select.Trigger>
            <Select.Content>
                <Select.Group>
                    {#each actions as a (a)}
                        <Select.Item
                            value={a}
                            label={a}
                            class="text-xs capitalize">
                            {a}
                        </Select.Item>
                    {/each}
                </Select.Group>
            </Select.Content>
        </Select.Root>

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
                    {onaction}
                    {ondelete}
                    {ondropInto} />
            {/each}
        </div>
    {/if}
</div>
