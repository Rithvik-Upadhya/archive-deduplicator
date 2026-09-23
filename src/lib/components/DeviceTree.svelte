<script lang="ts">
    import { getTree } from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import { deviceCollapsed, deviceFilterOn } from '$lib/stores/treeExpansion.svelte';
    import type { Source, TreeNode } from '$lib/types';
    import { formatBytes, pct, sourceColor, widestBytesWidth } from '$lib/util';
    import type { TreeSelection } from '$lib/stores/selection.svelte';
    import TreeItem from './TreeItem.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import { search } from '$lib/stores/search.svelte';

    // Kept in lockstep with TreeItem.svelte's DragMeta.
    type DragMeta = {
        name: string;
        type: TreeNode['type'];
        source_id: number;
        cross_dup: boolean;
    };

    interface Props {
        source: Source;
        onlocate?: (node: TreeNode) => void;
        draggable?: boolean;
        selection?: TreeSelection<DragMeta>;
    }

    let {
        source,
        onlocate,
        draggable = false,
        selection,
    }: Props = $props();

    let roots = $state<TreeNode[] | null>(null);
    let rootsError = $state(false);
    const expanded = $derived(!deviceCollapsed.has(source.id));
    let editing = $state(false);
    /** Populated when a rename begins. */
    let editValue = $state('');
    let confirmingDelete = $state(false);
    let deleteBusy = $state(false);
    /** Hides nodes whose only duplicates live on another device. */
    const filterCrossDevice = $derived(deviceFilterOn.has(source.id));

    // The funnel and the search are both the user's own filters, so both
    // apply: a match the funnel hides stays hidden.
    const visibleRoots = $derived(
        roots?.filter(
            n =>
                (!filterCrossDevice || !n.cross_dup) &&
                (!search.active || search.deviceVisible.has(n.id))
        ) ?? null
    );
    // A count counts *names* -- canonical files, hardlink aliases and symlinks
    // alike -- so it matches what the user would find inspecting the source by
    // hand. `cross_dup_file_count` is name-based too (an alias inherits its
    // canonical's `cross_dup`), so both sides of the subtraction agree.
    // Sizes are the other axis: an alias is a name with zero marginal bytes,
    // so `physical_size`/`cross_dup_size` stay canonical-only.
    const visibleFileCount = $derived(
        filterCrossDevice
            ? source.file_count - source.cross_dup_file_count
            : source.file_count
    );
    const visibleSize = $derived(
        filterCrossDevice
            ? source.physical_size - source.cross_dup_size
            : source.physical_size
    );
    // The hardlink annotation has to describe the same set as the two numbers
    // beside it. Unfiltered that is the whole device; with the funnel on it is
    // only the aliases still on screen -- often none at all, since an alias is
    // hidden whenever its canonical is, and then the annotation disappears
    // rather than claiming GBs of hardlinks among nothing.
    const visibleAliasBytes = $derived(
        filterCrossDevice
            ? source.alias_bytes - source.cross_dup_alias_bytes
            : source.alias_bytes
    );

    // Column widths for the tree below, derived from this device's own totals
    // and published as CSS variables on the card so they inherit all the way
    // down TreeItem's recursion (and reach the label cells in the strip).
    //
    // The source row bounds every figure any descendant can show: no subtree
    // holds more names than the device, and none holds more bytes. Sizing from
    // the data rather than a fixed guess is what closes the dead space between
    // columns. `widestBytesWidth` rather than `formatBytes(...).length` because
    // that width is not monotonic -- see its doc comment.
    //
    // The `ch` unit is the width of "0"; the small pad absorbs the letters in a
    // unit suffix, which are wider than a tabular digit.
    //
    // The `physical_size || total_size` fallback is load-bearing, not
    // defensive, and mirrors `source_list` in commands.rs: that column was
    // added with DEFAULT 0 and `migrate` never backfills it, so sources
    // imported before it still carry 0 -- which would size this column to
    // "0 B" and clip every real figure under it.
    const colCount = $derived(String(source.file_count).length);
    const colSize = $derived(
        widestBytesWidth(source.physical_size || source.total_size)
    );
    const colVars = $derived(
        `--col-count: calc(${colCount}ch + 0.25rem); --col-size: calc(${colSize}ch + 0.5rem)`
    );

    async function load() {
        rootsError = false;
        try {
            roots = await getTree(app.activeWorkspaceId!, source.id, null);
        } catch (err) {
            rootsError = true;
            taskTray.notify('Failed to load tree', 'error', String(err));
        }
    }

    /** Tracks which `treeVersion` `roots` reflects, so a dedup rerun (or any
     *  other workspace reload) invalidates the cache without discarding it
     *  on every unrelated re-render. */
    let loadedVersion = -1;
    $effect(() => {
        if (app.treeVersion !== loadedVersion) {
            loadedVersion = app.treeVersion;
            roots = null;
        }
        if (expanded && roots === null) load();
    });

    async function saveLabel() {
        editing = false;
        const name = editValue.trim();
        if (name && name !== source.device_label) {
            try {
                await app.renameDevice(source.id, name);
            } catch (err) {
                taskTray.notify('Rename failed', 'error', String(err));
            }
        }
    }

    /** Live value while the native picker is open. `input` fires on every
     *  drag tick, so it only previews; the pick is persisted once, on `change`. */
    let colorPreview = $state<string | null>(null);
    const swatch = $derived(colorPreview ?? sourceColor(source));

    async function saveColor(color: string) {
        try {
            await app.setSourceColor(source.id, color);
        } catch (err) {
            taskTray.notify('Colour change failed', 'error', String(err));
        } finally {
            // On success `source.color` now holds the pick; on failure this
            // reverts the swatch to what is actually stored.
            colorPreview = null;
        }
    }

    async function confirmDelete() {
        deleteBusy = true;
        try {
            await app.deleteSource(source.id);
            confirmingDelete = false;
            taskTray.notify(
                'Device removed',
                'success',
                `Removed “${source.device_label}”.`
            );
        } catch (err) {
            taskTray.notify('Remove failed', 'error', String(err));
        } finally {
            deleteBusy = false;
        }
    }
</script>

<div
    class="mb-2 flex flex-col rounded-md border bg-card"
    class:grow={expanded}
    style={colVars}>
    <div class="flex shrink-0 items-center gap-1.5 bg-muted/50 px-2 py-1.5">
        <Button
            variant="ghost"
            size="icon"
            class="size-5 shrink-0 text-muted-foreground transition-transform duration-150"
            aria-label="Toggle device"
            onclick={() => deviceCollapsed.toggle(source.id)}>
            <Icon
                icon="ph:caret-right-bold"
                class={expanded
                    ? 'rotate-90 transition-transform'
                    : 'transition-transform'} />
        </Button>
        <Icon icon="ph:hard-drive-fill" class="shrink-0 text-brand" />
        <!-- Name and swatch share one flexible slot so the swatch sits right
             after the name rather than at the far end of the row. -->
        <div class="flex min-w-0 flex-1 items-center gap-1.5">
            {#if editing}
                <!-- svelte-ignore a11y_autofocus -->
                <Input
                    autofocus
                    class="h-7 flex-1 text-sm"
                    bind:value={editValue}
                    onblur={saveLabel}
                    onkeydown={e => {
                        if (e.key === 'Enter') saveLabel();
                        if (e.key === 'Escape') editing = false;
                    }} />
            {:else}
                <button
                    type="button"
                    class="min-w-0 truncate text-start text-sm font-semibold hover:text-brand"
                    title="Rename device"
                    onclick={() => {
                        editing = true;
                        editValue = source.device_label;
                    }}>
                    {source.device_label}
                </button>
            {/if}
            <!-- Identity colour for the consolidated tree's source bars. The native
                 input is stretched invisibly over the swatch so a click on it opens
                 the OS picker. -->
            <label
                class="relative size-3.5 shrink-0 cursor-pointer rounded-full ring-1 ring-border"
                style="background: {swatch}"
                title="Source colour">
                <input
                    type="color"
                    class="absolute inset-0 size-full cursor-pointer opacity-0"
                    aria-label="Source colour"
                    value={swatch}
                    oninput={e => (colorPreview = e.currentTarget.value)}
                    onchange={e => saveColor(e.currentTarget.value)} />
            </label>
        </div>

        {#if !filterCrossDevice}
            <Badge
                variant="outline"
                class="shrink-0 px-1.5 py-0 font-heading text-[0.65rem] tabular-nums"
                title="Percentage of bytes duplicated elsewhere">
                {pct(source.duplicated_pct)}% dup
            </Badge>
        {/if}
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 {filterCrossDevice
                ? 'text-brand'
                : 'text-muted-foreground'}"
            aria-label={filterCrossDevice
                ? 'Disable filter to include duplicated files'
                : 'Hide files that have a duplicate on another device'}
            aria-pressed={filterCrossDevice}
            title={filterCrossDevice
                ? 'Disable filter to include duplicated files'
                : 'Hide files that have a duplicate on another device'}
            onclick={() => deviceFilterOn.toggle(source.id)}>
            <Icon icon={filterCrossDevice ? 'ph:funnel-fill' : 'ph:funnel'} />
        </Button>
        <!-- Destructive tint is held back until hover so the only permanently
             red things on screen are the ones reporting duplicate findings. -->
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-destructive"
            aria-label="Remove device"
            disabled={deleteBusy}
            onclick={() => (confirmingDelete = true)}>
            <Icon icon="ph:trash-fill" />
        </Button>
    </div>

    <!-- The summary doubles as the column header for the tree below: its right
         half was empty, and it already sits directly above the rows. `pe-2.5`
         (0.625rem) is not arbitrary -- it puts this strip's end edge exactly on
         a row's, which is the tree body's `px-1` plus each row's own `px-1.5`.
         `ps-7` stays: that indent lines the summary up under the device name
         and has nothing to do with the columns.

         `items-end` so the labels stay on the bottom line, right above the
         columns they name, if the summary text wraps. -->
    <div
        class="flex shrink-0 items-end gap-1.5 ps-7 pe-2.5 pt-2 pb-1 border-b-1 font-heading text-xs tabular-nums text-muted-foreground">
        <div class="min-w-0 flex-1">
            {visibleFileCount} files · {formatBytes(visibleSize)}
            {#if source.kind === 'scan'}· scanned{/if}
            {#if visibleAliasBytes > 0}
                <!-- No minus sign: the size to the left is already
                     `physical_size`, so these bytes have been deducted from it,
                     not from what the reader is looking at. A leading `-` read
                     as a second subtraction still to apply. -->
                <span
                    class="text-muted-foreground/70"
                    title="Of the {visibleFileCount} files shown, {formatBytes(
                        visibleAliasBytes
                    )} is hardlink aliases -- the same physical bytes under more than one name. The {formatBytes(
                        visibleSize
                    )} shown already counts those bytes once.">
                    ({formatBytes(visibleAliasBytes)} hardlinked, counted once)
                </span>
            {/if}
        </div>

        <!-- Column labels for the tree. Only while it is open: headings over a
             collapsed tree label nothing. The trailing cells are empty because
             the action columns need no label, but they must still be here or
             the labels land a column too far right -- and the locate cell must
             track `onlocate` exactly as TreeItem's does, or the labels shift by
             20px in any tree without a locate column. -->
        {#if expanded}
            <span class="w-(--col-count) shrink-0 text-right">files</span>
            <span class="w-(--col-size) shrink-0 text-right">size</span>
            <span class="w-12 shrink-0 text-right">dup %</span>
            <span class="w-5 shrink-0"></span>
            {#if onlocate}
                <span class="w-5 shrink-0"></span>
            {/if}
        {/if}
    </div>

    {#if expanded}
        <ScrollArea class="min-h-0 grow" scrollbarYClasses="w-2">
            <div class="px-1 pb-1 mt-1">
                {#if roots === null && rootsError}
                    <div
                        class="flex items-center gap-1.5 px-2 py-1.5 text-xs text-destructive">
                        <Icon icon="ph:warning-circle-fill" />
                        Failed to load.
                        <button
                            type="button"
                            class="underline hover:no-underline"
                            onclick={load}>Retry</button>
                    </div>
                {:else if roots === null}
                    <div
                        class="flex items-center gap-1.5 px-2 py-1.5 text-xs text-muted-foreground">
                        <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
                        Loading…
                    </div>
                {:else if roots.length === 0}
                    <div class="px-2 py-1.5 text-xs text-muted-foreground">
                        Empty
                    </div>
                {:else if visibleRoots && visibleRoots.length === 0}
                    <div class="px-2 py-1.5 text-xs text-muted-foreground">
                        {search.active
                            ? 'No matches on this device'
                            : 'All files on this device have duplicates elsewhere'}
                    </div>
                {:else}
                    {#each visibleRoots ?? [] as node (node.id)}
                        <TreeItem
                            {node}
                            workspaceId={app.activeWorkspaceId!}
                            {onlocate}
                            {draggable}
                            {selection}
                            {filterCrossDevice} />
                    {/each}
                {/if}
            </div>
        </ScrollArea>
    {/if}
</div>

<AlertDialog.Root bind:open={confirmingDelete}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Remove this device?</AlertDialog.Title>
            <AlertDialog.Description>
                “{source.device_label}” and its scanned tree will be removed
                from this workspace. Duplicate results will need re-running.
            </AlertDialog.Description>
        </AlertDialog.Header>
        <AlertDialog.Footer>
            <AlertDialog.Cancel disabled={deleteBusy}>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                disabled={deleteBusy}
                onclick={confirmDelete}
                class="bg-destructive text-white hover:bg-destructive/90">
                {#if deleteBusy}
                    <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
                {/if}
                {deleteBusy ? 'Removing…' : 'Remove'}
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
