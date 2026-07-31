<script lang="ts">
    import { getTree } from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { TreeNode } from '$lib/types';
    import { DUP_BADGE, dupLevel, formatBytes, pct } from '$lib/util';
    import Self from './TreeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Badge } from '$lib/components/ui/badge';
    import { Button } from '$lib/components/ui/button';

    interface Props {
        node: TreeNode;
        workspaceId: number;
        /** Called when a node is picked for the duplicate review panel. */
        onselect?: (node: TreeNode) => void;
        /** Called by the "locate duplicate" button to reveal the match group. */
        onlocate?: (node: TreeNode) => void;
        /** Node id currently focused in the review panel. */
        selectedId?: number | null;
        /** Hide nodes already ruled out as surplus copies. */
        hideResolved?: boolean;
        /** Enables HTML5 drag so nodes can be dropped into a consolidation tree. */
        draggable?: boolean;
    }

    let {
        node,
        workspaceId,
        onselect,
        onlocate,
        selectedId = null,
        hideResolved = false,
        draggable = false,
    }: Props = $props();

    let expanded = $state(false);
    let children = $state<TreeNode[] | null>(null);
    let loading = $state(false);

    const isDir = $derived(node.type === 'directory');
    const showLocate = $derived(
        !!onlocate && (node.has_duplicate || (isDir && node.dup_pct > 0))
    );
    const marked = $derived(app.effectiveMark(node));
    const selected = $derived(node.id === selectedId);
    /** Rows the user has ruled out, hidden when the caller asks for a clean tree. */
    const hidden = $derived(hideResolved && marked?.mark === 'drop');

    async function expand() {
        expanded = !expanded;
        if (expanded && children === null) {
            loading = true;
            try {
                children = await getTree(workspaceId, node.source_id, node.id);
            } finally {
                loading = false;
            }
        }
    }

    /** Clicking the row focuses the review panel. Directories select *and*
     *  expand, since a folder's duplicates live in the files inside it. */
    function activate() {
        onselect?.(node);
        if (isDir && !expanded) expand();
    }

    function onDragStart(e: DragEvent) {
        if (!e.dataTransfer) return;
        e.dataTransfer.setData(
            'application/x-dedup-node',
            JSON.stringify({
                node_id: node.id,
                name: node.name,
                type: node.type,
            })
        );
        e.dataTransfer.effectAllowed = 'copy';
    }
</script>

{#if !hidden}
<div class="text-sm">
    <div
        class="group/row flex cursor-pointer items-center gap-1.5 rounded-sm px-1.5 py-0.5 select-none hover:bg-accent hover:text-accent-foreground data-[selected=true]:bg-brand/15 data-[drop=true]:opacity-50"
        data-dup={node.has_duplicate}
        data-selected={selected}
        data-drop={marked?.mark === 'drop'}
        {draggable}
        role="treeitem"
        aria-selected={selected}
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        ondragstart={draggable ? onDragStart : undefined}
        onclick={activate}
        onkeydown={e => e.key === 'Enter' && activate()}>
        <!-- The caret is its own hit area so a folder can be collapsed without
             changing the review scope. The row itself remains the keyboard
             path, so the caret stays out of the tab order. -->
        {#if isDir}
            <button
                type="button"
                tabindex={-1}
                class="inline-flex w-3 shrink-0 justify-center text-muted-foreground transition-transform duration-150"
                class:rotate-90={expanded}
                aria-label={expanded ? 'Collapse' : 'Expand'}
                onclick={e => {
                    e.stopPropagation();
                    expand();
                }}>
                <Icon icon="ph:caret-right-bold" />
            </button>
        {:else}
            <span class="inline-flex w-3 shrink-0" aria-hidden="true"></span>
        {/if}
        <!-- Icons stay neutral: the badge to the right already reports the
             duplicate state, and tinting both turned the tree into a red wall. -->
        <Icon
            icon={isDir ? 'ph:folder-fill' : 'ph:file-fill'}
            class="shrink-0 text-muted-foreground" />
        {#if marked}
            <!-- Inherited marks render dimmer than ones set on this row. -->
            <span
                class="inline-flex shrink-0 {marked.mark === 'keep'
                    ? 'text-brand'
                    : 'text-muted-foreground'} {marked.explicit
                    ? ''
                    : 'opacity-60'}"
                title={marked.explicit
                    ? marked.mark === 'keep'
                        ? 'Marked as the copy to keep'
                        : 'Marked as a surplus copy'
                    : `Inherited “${marked.mark}” from a parent folder`}>
                <Icon
                    icon={marked.mark === 'keep'
                        ? 'ph:star-fill'
                        : 'ph:prohibit-bold'} />
            </span>
        {/if}
        <span
            class="truncate"
            class:line-through={marked?.mark === 'drop'}
            title={node.rel_path}>{node.name}</span>

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
                        {selectedId}
                        {hideResolved}
                        {draggable} />
                {/each}
            {/if}
        </div>
    {/if}
</div>
{/if}
