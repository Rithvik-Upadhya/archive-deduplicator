<script lang="ts">
    import { getTree } from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import { deviceCollapsed, deviceFilterOn } from '$lib/stores/treeExpansion.svelte';
    import type { Source, TreeNode } from '$lib/types';
    import { DUP_BADGE, dupLevel, formatBytes, pct } from '$lib/util';
    import type { TreeSelection } from '$lib/stores/selection.svelte';
    import TreeItem from './TreeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import { taskTray } from '$lib/stores/tasks.svelte';

    type DragMeta = { name: string; type: TreeNode['type'] };

    interface Props {
        source: Source;
        onselect?: (node: TreeNode) => void;
        onlocate?: (node: TreeNode) => void;
        draggable?: boolean;
        selection?: TreeSelection<DragMeta>;
    }

    let {
        source,
        onselect,
        onlocate,
        draggable = false,
        selection,
    }: Props = $props();

    let roots = $state<TreeNode[] | null>(null);
    const expanded = $derived(!deviceCollapsed.has(source.id));
    let editing = $state(false);
    /** Populated when a rename begins. */
    let editValue = $state('');
    let confirmingDelete = $state(false);
    /** Hides nodes whose only duplicates live on another device. */
    const filterCrossDevice = $derived(deviceFilterOn.has(source.id));

    const visibleRoots = $derived(
        roots?.filter(n => !filterCrossDevice || !n.cross_dup) ?? null
    );
    const visibleFileCount = $derived(
        filterCrossDevice
            ? source.file_count - source.cross_dup_file_count
            : source.file_count
    );
    // `physical_size` (total_size minus hardlink-alias bytes) is the real
    // disk usage; `cross_dup_size` is already computed against canonical
    // (non-alias) files only, so it composes cleanly with either base.
    const visibleSize = $derived(
        filterCrossDevice
            ? source.physical_size - source.cross_dup_size
            : source.physical_size
    );

    async function load() {
        roots = await getTree(app.activeWorkspaceId!, source.id, null);
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

    async function confirmDelete() {
        confirmingDelete = false;
        try {
            await app.deleteSource(source.id);
            taskTray.notify(
                'Device removed',
                'success',
                `Removed “${source.device_label}”.`
            );
        } catch (err) {
            taskTray.notify('Remove failed', 'error', String(err));
        }
    }
</script>

<div class="mb-2 flex flex-col rounded-md border bg-card" class:grow={expanded}>
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
                class="min-w-0 flex-1 truncate text-start text-sm font-semibold hover:text-brand"
                title="Rename device"
                onclick={() => {
                    editing = true;
                    editValue = source.device_label;
                }}>
                {source.device_label}
            </button>
        {/if}

        {#if !filterCrossDevice}
            <Badge
                variant="outline"
                class="shrink-0 px-1.5 py-0 font-heading text-[0.65rem] tabular-nums {DUP_BADGE[
                    dupLevel(pct(source.duplicated_pct))
                ]}"
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
            onclick={() => (confirmingDelete = true)}>
            <Icon icon="ph:trash-fill" />
        </Button>
    </div>

    <div
        class="shrink-0 px-2 pt-2 pb-1 ps-7 border-b-1 font-heading text-xs tabular-nums text-muted-foreground">
        {visibleFileCount} files · {formatBytes(visibleSize)}
        {#if source.kind === 'scan'}· scanned{/if}
        {#if source.alias_bytes > 0}
            <span
                class="text-muted-foreground/70"
                title="{formatBytes(
                    source.alias_bytes
                )} of this device's logical size comes from hardlink aliases -- the same physical bytes under more than one name. Counted once here.">
                (-{formatBytes(source.alias_bytes)} hardlinked, counts once)
            </span>
        {/if}
    </div>

    {#if expanded}
        <ScrollArea class="min-h-0 grow" scrollbarYClasses="w-2">
            <div class="px-1 pb-1 mt-1">
                {#if roots === null}
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
                        All files on this device have duplicates elsewhere
                    </div>
                {:else}
                    {#each visibleRoots ?? [] as node (node.id)}
                        <TreeItem
                            {node}
                            workspaceId={app.activeWorkspaceId!}
                            {onselect}
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
            <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                onclick={confirmDelete}
                class="bg-destructive text-white hover:bg-destructive/90">
                Remove
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
