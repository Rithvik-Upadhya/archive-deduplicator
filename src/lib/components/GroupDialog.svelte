<!-- A device-tree node's match group, shown in place by the "locate
     duplicates" button in both the Deduplicate and Consolidate views. Looks
     the group up itself, so neither view repeats the fetch. A member's arrow
     closes the dialog and reveals that member in its device tree. -->
<script lang="ts">
    import { getGroupForNode } from '$lib/api';
    import type { MatchGroup, TreeNode } from '$lib/types';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import GroupMembers from './GroupMembers.svelte';
    import * as Dialog from '$lib/components/ui/dialog';

    interface Props {
        open: boolean;
        node: TreeNode | null;
    }

    let { open = $bindable(), node }: Props = $props();

    let group = $state<MatchGroup | null>(null);
    let loading = $state(false);

    $effect(() => {
        if (!open || !node) return;
        const id = node.id;
        group = null;
        loading = true;
        getGroupForNode(id)
            .then(g => {
                // A later click may have replaced this lookup; keep the newest.
                if (node?.id === id) group = g;
            })
            .catch(err => {
                if (node?.id !== id) return;
                open = false;
                taskTray.notify('Failed to load duplicates', 'error', String(err));
            })
            .finally(() => {
                if (node?.id === id) loading = false;
            });
    });
</script>

<Dialog.Root bind:open>
    <Dialog.Content class="sm:max-w-2xl">
        <Dialog.Header>
            <Dialog.Title class="truncate">
                Duplicates of: {node?.name}
            </Dialog.Title>
            <Dialog.Description class="truncate">
                {node?.rel_path}
            </Dialog.Description>
        </Dialog.Header>
        <div class="max-h-[60vh] overflow-y-auto">
            {#if loading}
                <p class="text-sm text-muted-foreground">Loading…</p>
            {:else if group}
                <GroupMembers
                    {group}
                    selfId={node?.id ?? null}
                    onreveal={() => (open = false)} />
            {:else}
                <p class="text-sm text-muted-foreground">
                    No duplicate group for this {node?.type === 'directory'
                        ? 'folder'
                        : 'file'}.
                </p>
            {/if}
        </div>
    </Dialog.Content>
</Dialog.Root>
