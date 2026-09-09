<script lang="ts">
    import { getTree } from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import { deviceTreeExpanded } from '$lib/stores/treeExpansion.svelte';
    import type { NodeType, TreeNode } from '$lib/types';
    import {
        copyPath,
        DUP_TONE,
        formatBytes,
        pct,
    } from '$lib/util';
    import { selectable, type TreeSelection } from '$lib/stores/selection.svelte';
    import Self from './TreeItem.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Badge } from '$lib/components/ui/badge';
    import { Button } from '$lib/components/ui/button';

    type DragMeta = {
        name: string;
        type: NodeType;
        source_id: number;
        /** Whether this node's only duplicates live on another device --
         *  lets the consolidation drop handler honour each source's funnel. */
        cross_dup: boolean;
    };

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
        /** Hides nodes whose only duplicates live on another device. */
        filterCrossDevice?: boolean;
    }

    let {
        node,
        workspaceId,
        onselect,
        onlocate,
        draggable = false,
        selection,
        filterCrossDevice = false,
    }: Props = $props();

    const expanded = $derived(deviceTreeExpanded.has(node.id));
    let children = $state<TreeNode[] | null>(null);
    let loading = $state(false);

    const visibleChildren = $derived(
        children?.filter(c => !filterCrossDevice || !c.cross_dup) ?? null
    );

    const isDir = $derived(node.type === 'directory');
    const visibleFileCount = $derived(
        filterCrossDevice
            ? node.subtree_file_count - node.cross_dup_file_count
            : node.subtree_file_count
    );
    const visibleSize = $derived(
        filterCrossDevice ? node.subtree_size - node.cross_dup_size : node.subtree_size
    );
    // Red when *any* name beneath this folder has a copy on another device.
    //
    // Deliberately a count, not `node.cross_dup`: that flag is all-or-nothing
    // for a directory (rollup.rs's `dir_cross_dup` needs *every* leaf to be
    // cross-source), so a folder of ninety-nine unique files and one duplicate
    // read grey -- "all exclusive to this device", which was false about the
    // one file the user actually needed to see. `cross_dup_file_count` is
    // rolled up the entire ancestor chain, so one deeply nested duplicate
    // reddens every folder above it, and it counts names -- aliases and
    // symlinks included -- which is the right axis for "even one file".
    //
    // It also drops `cross_dup`'s vacuous truth for a leaf-less directory
    // (`0 == 0`, unguarded in rollup.rs): an empty folder has a count of 0.
    const folderTone = $derived(
        node.cross_dup_file_count > 0 ? 'external' : 'internal'
    );
    // The badge can read 0% for two different reasons, and the note has to say
    // which: the duplicated content genuinely holds no bytes (symlinks,
    // hardlink aliases, empty files), or it holds so few that one decimal place
    // rounds it away. Only append it when the displayed figure is actually 0.
    const folderDupPct = $derived(pct(node.dup_pct));
    // Said on every folder badge, because the number alone does not say what it
    // is a share *of*. Note it deliberately does not claim to be the
    // cross-device share: `dup_pct` pools duplication of both kinds, while the
    // red/grey tone is about cross-device only. Two different questions in one
    // row, so the tooltip has to be explicit about which the number answers.
    const PCT_BASIS =
        " The percentage is the total size of duplicated content here as a share of this folder's size, counting copies on this device as well as on others.";
    const zeroPctNote = $derived(
        folderDupPct !== 0
            ? ''
            : node.dup_pct > 0
              ? ' It rounds to 0% here: those duplicates are a negligible fraction of the folder.'
              : ' It is 0% here because those copies hold no bytes of their own -- symlinks, hardlink aliases and empty files.'
    );
    const folderBadgeTitle = $derived(
        (folderTone === 'external'
            ? `${node.cross_dup_file_count} item${
                  node.cross_dup_file_count === 1 ? '' : 's'
              } under this folder also exist${
                  node.cross_dup_file_count === 1 ? 's' : ''
              } on another device.`
            : 'Duplicated within this device only -- every copy of everything under this folder is on this device.') +
        PCT_BASIS +
        zeroPctNote
    );
    const copyLabel = $derived(
        isDir ? 'Copy folder path' : 'Copy file path'
    );
    const showLocate = $derived(
        !!onlocate && (isDir ? node.in_folder_group : node.has_duplicate)
    );

    async function loadChildren() {
        loading = true;
        try {
            children = await getTree(workspaceId, node.source_id, node.id);
        } finally {
            loading = false;
        }
    }

    async function toggleExpanded() {
        const willExpand = !expanded;
        deviceTreeExpanded.toggle(node.id);
        if (isDir && willExpand && children === null) {
            await loadChildren();
        }
    }

    /** Tracks which `treeVersion` `children` reflects, so a dedup rerun (or
     *  any other workspace reload) invalidates the cache and refetches it
     *  while the node stays expanded, instead of leaving it stale. Collapsing
     *  still leaves `children` cached for a free re-expand as long as the
     *  version hasn't moved. */
    let loadedVersion = -1;
    $effect(() => {
        if (app.treeVersion !== loadedVersion) {
            loadedVersion = app.treeVersion;
            children = null;
        }
        if (isDir && expanded && children === null && !loading) {
            loadChildren();
        }
    });

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
                ? {
                      node_id: id,
                      name: node.name,
                      type: node.type,
                      source_id: node.source_id,
                      cross_dup: node.cross_dup,
                  }
                : {
                      node_id: id,
                      name: meta?.name ?? '',
                      type: meta?.type ?? 'file',
                      source_id: meta?.source_id ?? node.source_id,
                      cross_dup: meta?.cross_dup ?? false,
                  };
        });
        e.dataTransfer.setData('application/x-dedup-node', JSON.stringify({ items }));
        e.dataTransfer.effectAllowed = 'copy';
    }
</script>

<div class="text-sm">
    <div
        class="group/row flex cursor-pointer items-center gap-1.5 px-1.5 py-0.5 select-none hover:bg-accent hover:text-accent-foreground data-[selected=true]:bg-accent data-[selected=true]:text-accent-foreground"
        data-dup={node.has_duplicate}
        data-selected={selection?.isSelected(node.id) ?? false}
        {draggable}
        role="treeitem"
        aria-selected={selection?.isSelected(node.id) ?? false}
        aria-expanded={isDir ? expanded : undefined}
        tabindex="0"
        use:selectable={{
            selection,
            id: node.id,
            meta: {
                name: node.name,
                type: node.type,
                source_id: node.source_id,
                cross_dup: node.cross_dup,
            },
        }}
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
        <!-- Name and the skipped marker share one flexible slot. The marker is
             rare, so giving it a fixed column of its own would shorten every
             name in the tree for something usually absent; here it costs width
             only on the rows that actually carry it, and still lands just left
             of the count column. -->
        <div class="flex min-w-0 flex-1 items-center gap-1.5">
            <span class="truncate" title={node.rel_path}>{node.name}</span>
            <!-- `--warn` carries the "uncertain" data semantic (see app.css):
                 these nodes are neither confirmed duplicates nor confirmed
                 unique, which is exactly what that token is for. -->
            {#if isDir && node.skipped_count > 0}
                <span
                    class="ms-auto shrink-0 inline-flex items-center gap-0.5 font-heading text-[0.65rem] tabular-nums whitespace-nowrap text-warn"
                    title="{node.skipped_count} item{node.skipped_count === 1
                        ? ''
                        : 's'} here could not be compared: a safety limit in the matcher stopped it judging an unusually large group of same-named, same-sized files. They are neither confirmed duplicates nor confirmed unique.">
                    <Icon icon="ph:warning-circle-fill" />
                    {node.skipped_count} skipped
                </span>
            {:else if !isDir && node.skipped}
                <!-- Icon only, no label: a filename needs the width more than
                     this does, and the tooltip carries the meaning. -->
                <span
                    class="ms-auto shrink-0 text-warn"
                    title="Skipped: a safety limit stopped the matcher judging this file, so it is neither a confirmed duplicate nor confirmed unique.">
                    <Icon icon="ph:warning-circle-fill" />
                </span>
            {/if}
        </div>

        <!-- Fixed-width columns from here on, each rendered whether or not it
             has content: a slot that vanishes mid-cluster shifts everything
             left of it, which is what made this ragged before. The right edges
             already coincide across nesting depths, because the indent wrappers
             below inset only the start edge.

             The count is the one exception, and only because it is the
             *leftmost* column: a file has no count to show, and everything to
             the right of it is fixed-width and anchored to that shared right
             edge, so omitting it shifts nothing and hands the width back to the
             filename. Folder counts still line up with each other and with the
             strip's label. Do not extend this to any column further right. -->
        {#if isDir}
            <span
                class="w-(--col-count) shrink-0 text-right font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
                {visibleFileCount}
            </span>
        {/if}
        <span
            class="w-(--col-size) shrink-0 text-right font-heading text-xs tabular-nums whitespace-nowrap text-muted-foreground">
            {formatBytes(isDir ? visibleSize : node.size)}
        </span>

        <!-- Fixed rather than data-driven like the two above: the content is
             five characters at most either way (`43.2%`, `100%`, `dup`,
             `link`), so measuring it would be machinery for no gain. -->
        <span class="flex w-12 shrink-0 justify-end">
            {#if isDir}
                <!-- The count is in the gate as well as the tone. `dup_pct` is a
                     share of *bytes*, so a folder whose only cross-source
                     entries are symlinks, hardlink aliases or empty files has
                     none -- and those are exactly the folders the count rule
                     exists to catch, so gating on bytes alone would silently
                     hide them. -->
                {#if !filterCrossDevice && (node.dup_pct > 0 || node.cross_dup_file_count > 0)}
                    <Badge
                        variant="outline"
                        class="shrink-0 px-1 py-0 font-heading text-[0.65rem] tabular-nums {DUP_TONE[
                            folderTone
                        ]}"
                        title={folderBadgeTitle}>
                        {folderDupPct}%
                    </Badge>
                {/if}
            {:else if node.is_hardlink}
                <Badge
                    variant="outline"
                    class="shrink-0 border-muted-foreground/45 px-1 py-0 font-heading text-[0.65rem] text-muted-foreground"
                    title="This is a hardlink: one physical file also known by another name on this device. Not a duplicate -- deleting one name doesn't free any space until every name is gone."
                    >link</Badge>
            {:else if node.has_duplicate}
                <Badge
                    variant="outline"
                    class="shrink-0 px-1 py-0 font-heading text-[0.65rem] {DUP_TONE[
                        node.cross_dup ? 'external' : 'internal'
                    ]}"
                    title={node.cross_dup
                        ? 'This file also exists on another device -- deleting it here would not lose the only copy'
                        : 'This file has a duplicate, but only on this device -- every copy is on this one shelf'}
                    >dup</Badge>
            {/if}
        </span>

        <!-- One column per button, not one shared slot. A shared slot with
             `justify-end` only pins the *last* item, so a folder with a copy
             button but no locate button put copy where locate belongs. -->
        <span class="flex w-5 shrink-0">
            <Button
                variant="ghost"
                size="icon"
                class="size-5 shrink-0 opacity-0 transition-opacity group-hover/row:opacity-100"
                title={copyLabel}
                onclick={e => {
                    e.stopPropagation();
                    // `rel_path` is relative to the scan root, so the copied
                    // path carries no device or source name. For a file it
                    // already ends in the filename.
                    copyPath(node.rel_path, app.pathSep);
                }}>
                <Icon icon="ph:copy-fill" />
                <span class="sr-only">{copyLabel}</span>
            </Button>
        </span>

        <span class="flex w-5 shrink-0">
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
        </span>
    </div>

    {#if expanded}
        <div class="ms-3 border-l border-border ps-1">
            {#if loading}
                <div
                    class="flex items-center gap-1.5 px-2 py-1 text-xs text-muted-foreground">
                    <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
                    Loading…
                </div>
            {:else if visibleChildren}
                {#each visibleChildren as child (child.id)}
                    <Self
                        node={child}
                        {workspaceId}
                        {onselect}
                        {onlocate}
                        {draggable}
                        {selection}
                        {filterCrossDevice} />
                {/each}
            {/if}
        </div>
    {/if}
</div>
