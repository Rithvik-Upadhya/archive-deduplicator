<script lang="ts">
    import type { PathTreeNode } from '$lib/types';
    import { type Relocation, type SourceBars } from '$lib/util';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import { consolidationTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import Self from './PathTreeItem.svelte';
    import SourceBarsCell from './SourceBarsCell.svelte';
    import NameMarks from './NameMarks.svelte';
    import RowMenu from './RowMenu.svelte';
    import Icon from '$lib/components/Icon.svelte';
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
        /** Internal-relocation marks for a row (see `util.relocationsFor`). */
        relocOf: (id: number) => Relocation;
        limit: number;
        selection: TreeSelection;
        /** Renamed items anywhere beneath a row. */
        renamedBelowOf: (id: number) => number;
        /** Struck items anywhere beneath a row. */
        struckBelowOf: (id: number) => number;
        /** Whether a row has a child still in the end state (not struck). */
        hasLiveKids: (id: number) => boolean;
        onrename: (node: PathTreeNode, newName: string) => void;
        onrevert: (node: PathTreeNode) => void;
        /** Open the new-folder prompt for a subfolder of this row. */
        onnewfolder: (node: PathTreeNode) => void;
        ondropInto: (parentId: number | null, e: DragEvent) => void;
    }

    let {
        node,
        childrenOf,
        pathOf,
        barsOf,
        relocOf,
        limit,
        selection,
        renamedBelowOf,
        struckBelowOf,
        hasLiveKids,
        onrename,
        onrevert,
        onnewfolder,
        ondropInto,
    }: Props = $props();

    const isDir = $derived(node.type === 'directory');
    // Where this node came from, in the same shape the Consolidate tree uses.
    // A folder the user created by hand has no source, so it falls back to its
    // own name. This is hover text only: the menu's "Copy new path"
    // deliberately yields the end-state path instead, with no device name.
    const origin = $derived(
        node.origin_device && node.origin_path
            ? `${node.origin_device}://${node.origin_path}`
            : node.name
    );
    // A renamed row keeps its original name in the tooltip, composed with the
    // origin rather than replacing it, so an edited row shows both. It used to
    // end "-- click to rename", which was wrong: renaming is in the row menu,
    // and clicking the row selects it.
    const nameTitle = $derived(
        node.edited
            ? `${origin}\nOriginally “${node.original_name}”`
            : origin
    );
    const kids = $derived(childrenOf(node.id));
    const bars = $derived(barsOf(node));
    const reloc = $derived(relocOf(node.id));
    // A leaf of the *end state*: live, with no live children. Only these are
    // measured (see pathfix.rs), so only these carry a path-length badge. A
    // live folder whose children are all struck is one.
    const isMeasuredLeaf = $derived(!node.struck && !hasLiveKids(node.id));
    // Over-limit is marked by an icon after the name, not by tinting the row,
    // so the only tone left is struck.
    const tone = $derived(node.struck ? 'text-struck' : '');
    // `over_limit` is set on an offending leaf *and* every ancestor of one
    // (pathfix.rs), so each folder on the way down carries the icon too.
    // Rows are never auto-expanded to show it: with thousands over the
    // limit, opening them all at once hung the view.
    const overTitle = $derived(
        isMeasuredLeaf
            ? `Path is ${node.path_length} characters, over the ${limit} limit`
            : `Something inside is over the ${limit}-character limit`
    );
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
            icon={isDir ? 'ph:folder-fill' : 'ph:file-fill'}
            class="shrink-0 {tone || 'text-muted-foreground'}" />

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
                    class="truncate {tone}"
                    class:line-through={node.struck}
                    title={nameTitle}>
                    {node.name}
                </span>
                <NameMarks
                    original={node.edited ? node.original_name : null}
                    renamedColor={bars.colors[0]}
                    renamedBelow={renamedBelowOf(node.id)}
                    struckBelow={!node.struck && struckBelowOf(node.id) > 0}
                    relocatedColor={reloc.color}
                    relocatedTitle={reloc.title}
                    movedBelow={reloc.movedBelow} />
                {#if node.over_limit}
                    <span
                        class="inline-flex shrink-0 text-destructive"
                        title={overTitle}
                        aria-label={overTitle}>
                        <Icon icon="ph:warning-circle-fill" class="size-3.5" />
                    </span>
                {/if}
            </span>
        {/if}

        {#if isMeasuredLeaf}
            <Badge
                variant={node.over_limit ? 'destructive' : 'secondary'}
                class="ms-auto shrink-0 px-1.5 py-0 text-[0.7rem]">
                {node.path_length} chars
            </Badge>
        {/if}

        <SourceBarsCell {bars} />

        <!-- Copy new path is deliberately not `origin`: hovering shows where
             this came from, but the end-state path is free of any device name. -->
        <RowMenu
            sourcePath={node.origin_path}
            newPath={() => pathOf(node.id)}
            renamed={node.edited}
            {isDir}
            onrename={startRename}
            onreset={() => onrevert(node)}
            onnewfolder={() => onnewfolder(node)} />
    </div>

    {#if isDir && expanded}
        <div class="ms-3 border-l border-border ps-1">
            {#each kids as child (child.id)}
                <Self
                    node={child}
                    {childrenOf}
                    {pathOf}
                    {barsOf}
                    {relocOf}
                    {limit}
                    {selection}
                    {renamedBelowOf}
                    {struckBelowOf}
                    {hasLiveKids}
                    {onrename}
                    {onrevert}
                    {onnewfolder}
                    {ondropInto} />
            {/each}
        </div>
    {/if}
</div>
