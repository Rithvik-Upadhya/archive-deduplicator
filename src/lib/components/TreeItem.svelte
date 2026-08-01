<script lang="ts">
    import { getTree } from '$lib/api';
    import type { NodeType, TreeNode } from '$lib/types';
    import { DUP_BADGE, dupLevel, formatBytes, pct } from '$lib/util';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import Self from './TreeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Badge } from '$lib/components/ui/badge';
    import { Button } from '$lib/components/ui/button';

    type DragMeta = { name: string; type: NodeType };

    interface Props {
        node: TreeNode;
        workspaceId: number;
        /** Called when a node is picked for the duplicate review panel. */
        onselect?: (node: TreeNode) => void;
        /** Called by the "locate duplicate" button to reveal the match group. */
        onlocate?: (node: TreeNode) => void;
        /** Enables HTML5 drag so nodes can be dropped into a consolidation tree. */
        draggable?: boolean;
        /** Shared multi-select state, only used when `draggable` is set --
         *  DedupView's browsing tree stays single-select via `onselect`. */
        selection?: TreeSelection<DragMeta>;
    }

    let {
        node,
        workspaceId,
        onselect,
        onlocate,
        draggable = false,
        selection,
    }: Props = $props();

    let expanded = $state(false);
    let children = $state<TreeNode[] | null>(null);
    let loading = $state(false);

    const isDir = $derived(node.type === 'directory');
    const showLocate = $derived(
        !!onlocate && (node.has_duplicate || (isDir && node.dup_pct > 0))
    );

    async function toggleExpanded() {
        expanded = !expanded;
        if (isDir && expanded && children === null) {
            loading = true;
            try {
                children = await getTree(workspaceId, node.source_id, node.id);
            } finally {
                loading = false;
            }
        }
    }

    function onRowActivate(e: MouseEvent | KeyboardEvent) {
        if (selection) {
            selection.click(node.id, e);
            return;
        }
        // Unchanged DedupView behavior: a directory row toggles expand: a
        // file row hands off to the duplicate review panel.
        if (isDir) {
            toggleExpanded();
        } else {
            onselect?.(node);
        }
    }

    function onCaretClick(e: MouseEvent) {
        e.stopPropagation();
        toggleExpanded();
    }

    function onDragStart(e: DragEvent) {
        if (!e.dataTransfer || !selection) return;
        const ids = selection.dragIds(node.id);
        const items = ids.map(id => {
            const meta = selection.getMeta(id);
            return id === node.id
                ? { node_id: id, name: node.name, type: node.type }
                : { node_id: id, name: meta?.name ?? '', type: meta?.type ?? 'file' };
        });
        e.dataTransfer.setData('application/x-dedup-node', JSON.stringify({ items }));
        e.dataTransfer.effectAllowed = 'copy';
    }
</script>

<div class="text-sm">
    <div
        class="group/row flex cursor-pointer items-center gap-1.5 rounded-sm px-1.5 py-0.5 select-none hover:bg-accent hover:text-accent-foreground data-[selected=true]:bg-accent data-[selected=true]:text-accent-foreground"
        data-dup={node.has_duplicate}
        data-selected={selection?.isSelected(node.id) ?? false}
        {draggable}
        role="treeitem"
        aria-selected={selection?.isSelected(node.id) ?? false}
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        use:selectable={{ selection, id: node.id, meta: { name: node.name, type: node.type } }}
        ondragstart={draggable ? onDragStart : undefined}
        onclick={onRowActivate}
        onkeydown={e => e.key === 'Enter' && onRowActivate(e)}>
        <button
            type="button"
            class="inline-flex w-3 shrink-0 justify-center text-muted-foreground transition-transform duration-150"
            class:rotate-90={expanded}
            class:invisible={!isDir}
            aria-label="Toggle"
            onclick={onCaretClick}>
            <Icon icon="ph:caret-right-bold" />
        </button>
        <!-- Icons stay neutral: the badge to the right already reports the
             duplicate state, and tinting both turned the tree into a red wall. -->
        <Icon
            icon={isDir ? 'ph:folder-fill' : 'ph:file-fill'}
            class="shrink-0 text-muted-foreground" />
        <span class="truncate" title={node.rel_path}>{node.name}</span>

        {#if isDir}
            <span
                class="ms-auto shrink-0 font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
                {node.subtree_file_count} files · {formatBytes(
                    node.subtree_size
                )}
            </span>
            {#if node.dup_pct > 0}
                <Badge
                    variant="outline"
                    class="shrink-0 px-1 py-0 font-heading text-[0.65rem] tabular-nums {DUP_BADGE[
                        dupLevel(node.dup_pct)
                    ]}"
                    title="Portion of this folder duplicated elsewhere">
                    {pct(node.dup_pct)}% dup
                </Badge>
            {/if}
        {:else}
            <span
                class="ms-auto shrink-0 font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
                {formatBytes(node.size)}
            </span>
            {#if node.has_duplicate}
                <Badge
                    variant="outline"
                    class="shrink-0 border-brand/45 px-1 py-0 font-heading text-[0.65rem] text-brand"
                    title="This file likely has duplicates">dup</Badge>
            {/if}
        {/if}

        {#if showLocate}
            <Button
                variant="ghost"
                size="icon"
                class="size-5 shrink-0 opacity-0 transition-opacity group-hover/row:opacity-100"
                title="Show where the duplicates are"
                onclick={e => {
                    e.stopPropagation();
                    onlocate?.(node);
                }}>
                <Icon icon="ph:magnifying-glass-bold" />
                <span class="sr-only">Locate duplicates</span>
            </Button>
        {/if}
    </div>

    {#if expanded}
        <div class="ms-3 border-l border-border ps-1">
            {#if loading}
                <div
                    class="flex items-center gap-1.5 px-2 py-1 text-xs text-muted-foreground">
                    <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
                    Loading…
                </div>
            {:else if children}
                {#each children as child (child.id)}
                    <Self
                        node={child}
                        {workspaceId}
                        {onselect}
                        {onlocate}
                        {draggable}
                        {selection} />
                {/each}
            {/if}
        </div>
    {/if}
</div>
