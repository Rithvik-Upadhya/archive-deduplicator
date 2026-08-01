<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { ConsolidationNode } from '$lib/types';
    import DeviceTree from './DeviceTree.svelte';
    import ConsolidationNodeItem from './ConsolidationNodeItem.svelte';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import * as Dialog from '$lib/components/ui/dialog';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import * as Field from '$lib/components/ui/field';
    import * as Empty from '$lib/components/ui/empty';
    import { toast } from 'svelte-sonner';

    let consolidationId = $state<number | null>(null);
    let nodes = $state<ConsolidationNode[]>([]);
    let rootDragOver = $state(false);

    let showNewFolderDialog = $state(false);
    let newFolderName = $state('New Folder');
    let deleteTarget = $state<ConsolidationNode | null>(null);

    async function load() {
        if (app.activeWorkspaceId == null) return;
        const [cid, cnodes] = await api.consolidationGet(app.activeWorkspaceId);
        consolidationId = cid;
        nodes = cnodes;
    }

    // Reload whenever the active workspace changes.
    let loadedFor = $state<number | null>(null);
    $effect(() => {
        if (
            app.activeWorkspaceId != null &&
            app.activeWorkspaceId !== loadedFor
        ) {
            loadedFor = app.activeWorkspaceId;
            load();
        }
    });

    function childrenOf(parentId: number | null): ConsolidationNode[] {
        return nodes
            .filter(n => n.parent_id === parentId)
            .sort((a, b) => a.sort_order - b.sort_order);
    }

    async function handleDrop(parentId: number | null, e: DragEvent) {
        const raw = e.dataTransfer?.getData('application/x-dedup-node');
        if (!raw || consolidationId == null || app.activeWorkspaceId == null)
            return;
        const payload = JSON.parse(raw) as {
            node_id: number;
            name: string;
            type: 'file' | 'directory' | 'link';
        };
        try {
            const created = await api.consolidationAddNode({
                consolidationId,
                parentId,
                name: payload.name,
                nodeType: payload.type === 'link' ? 'file' : payload.type,
                sourceNodeId: payload.node_id,
            });
            nodes = [...nodes, created];
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function confirmDelete() {
        const target = deleteTarget;
        deleteTarget = null;
        if (!target) return;
        // Remove the node and its descendants locally.
        const toRemove = new Set<number>([target.id]);
        let changed = true;
        while (changed) {
            changed = false;
            for (const n of nodes) {
                if (
                    n.parent_id != null &&
                    toRemove.has(n.parent_id) &&
                    !toRemove.has(n.id)
                ) {
                    toRemove.add(n.id);
                    changed = true;
                }
            }
        }
        try {
            await api.consolidationDeleteNode(target.id);
            nodes = nodes.filter(n => !toRemove.has(n.id));
        } catch (err) {
            toast.error(String(err));
        }
    }

    function openNewFolderDialog() {
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        newFolderName = 'New Folder';
        showNewFolderDialog = true;
    }

    async function submitNewFolder(e: SubmitEvent) {
        e.preventDefault();
        if (consolidationId == null || app.activeWorkspaceId == null) return;
        const name = newFolderName.trim();
        if (!name) return;
        showNewFolderDialog = false;
        try {
            const created = await api.consolidationAddNode({
                consolidationId,
                parentId: null,
                name,
                nodeType: 'directory',
                sourceNodeId: null,
            });
            nodes = [...nodes, created];
        } catch (err) {
            toast.error(String(err));
        }
    }
</script>

<div class="grid min-h-0 grow grid-cols-2 gap-4 overflow-hidden">
    <!-- Source devices -->
    <section class="flex min-h-0 min-w-0 flex-col overflow-hidden pe-1">
        <h2 class="section-label mb-2 shrink-0">
            Source devices — drag files &amp; folders →
        </h2>
        {#if app.sources.length === 0}
            <Empty.Root class="border border-dashed">
                <Empty.Header>
                    <Empty.Media variant="icon">
                        <Icon icon="ph:hard-drives-fill" />
                    </Empty.Media>
                    <Empty.Title>No devices</Empty.Title>
                    <Empty.Description>
                        Add devices first in the Deduplicate view.
                    </Empty.Description>
                </Empty.Header>
            </Empty.Root>
        {:else}
            {#each app.sources as s (s.id)}
                <DeviceTree source={s} draggable />
            {/each}
        {/if}
    </section>

    <!-- Consolidated target tree -->
    <section class="flex min-h-0 min-w-0 flex-col overflow-hidden">
        <div class="mb-2 flex items-center justify-between gap-2">
            <h2 class="section-label">Consolidated tree</h2>
            <Button variant="outline" size="sm" onclick={openNewFolderDialog}>
                <Icon icon="ph:folder-plus-fill" />
                <span>Folder</span>
            </Button>
        </div>
        <div
            class="min-h-32 grow overflow-y-auto rounded-md border-2 border-dashed p-2 transition-colors data-[drag=true]:border-brand data-[drag=true]:bg-brand/10"
            data-drag={rootDragOver}
            role="tree"
            aria-label="Consolidated tree"
            tabindex="0"
            ondragover={e => {
                e.preventDefault();
                rootDragOver = true;
            }}
            ondragleave={() => (rootDragOver = false)}
            ondrop={e => {
                e.preventDefault();
                rootDragOver = false;
                handleDrop(null, e);
            }}>
            {#if childrenOf(null).length === 0}
                <p class="p-4 text-center text-sm text-muted-foreground">
                    Drop files or folders here to build your target tree.
                </p>
            {:else}
                {#each childrenOf(null) as node (node.id)}
                    <ConsolidationNodeItem
                        {node}
                        {childrenOf}
                        ondelete={n => (deleteTarget = n)}
                        ondropInto={handleDrop} />
                {/each}
            {/if}
        </div>
    </section>
</div>

<Dialog.Root bind:open={showNewFolderDialog}>
    <Dialog.Content class="sm:max-w-sm">
        <form onsubmit={submitNewFolder}>
            <Dialog.Header>
                <Dialog.Title>New folder</Dialog.Title>
                <Dialog.Description>
                    Adds an empty folder at the root of the consolidated tree.
                </Dialog.Description>
            </Dialog.Header>
            <Field.FieldGroup class="py-4">
                <Field.Field>
                    <Field.FieldLabel for="folder-name">Name</Field.FieldLabel>
                    <!-- svelte-ignore a11y_autofocus -->
                    <Input
                        id="folder-name"
                        autofocus
                        bind:value={newFolderName} />
                </Field.Field>
            </Field.FieldGroup>
            <Dialog.Footer>
                <Button
                    type="button"
                    variant="outline"
                    onclick={() => (showNewFolderDialog = false)}
                    >Cancel</Button>
                <Button type="submit" disabled={!newFolderName.trim()}>
                    Create
                </Button>
            </Dialog.Footer>
        </form>
    </Dialog.Content>
</Dialog.Root>

<AlertDialog.Root
    open={deleteTarget !== null}
    onOpenChange={o => {
        if (!o) deleteTarget = null;
    }}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Remove from the plan?</AlertDialog.Title>
            <AlertDialog.Description>
                “{deleteTarget?.name}”{deleteTarget?.type === 'directory'
                    ? ' and everything inside it'
                    : ''} will be removed from the consolidated tree. Your source
                devices are not touched.
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
