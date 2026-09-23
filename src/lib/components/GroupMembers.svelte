<!-- One match group's confidence line and member list. Shared by the
     Deduplicate view's "Duplicates of" card and the Consolidate view's
     GroupDialog, so the two cannot drift. -->
<script lang="ts">
    import type { MatchGroup } from '$lib/types';
    import { app } from '$lib/stores/app.svelte';
    import {
        confidenceTone,
        copyPath,
        formatBytes,
        formatTime,
        pct,
    } from '$lib/util';
    import RevealButton from './RevealButton.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';

    interface Props {
        group: MatchGroup;
        /** The node the group was looked up from; its row is highlighted. */
        selfId: number | null;
        /** Called when a member's arrow is clicked, before it is revealed. */
        onreveal?: () => void;
    }

    let { group, selfId, onreveal }: Props = $props();

    // A folder group's members are folders and a file group's are files, so one
    // label per group covers every member row.
    const memberCopyLabel = $derived(
        group.kind === 'folder' ? 'Copy folder path' : 'Copy file path'
    );
</script>

<p class="mb-2 text-xs text-muted-foreground">
    <span
        class="font-heading font-semibold tabular-nums {confidenceTone(
            pct(group.confidence)
        )}">
        {pct(group.confidence)}%
    </span>
    · {group.primary_signal}
</p>
<ul class="flex flex-col">
    {#each group.members as m (m.node_id)}
        <li
            class="flex items-center gap-2 rounded-sm px-1 py-0.5 text-xs data-[self=true]:bg-brand/10"
            data-self={m.node_id === selfId}>
            <span
                class="shrink-0 font-medium whitespace-nowrap text-muted-foreground"
                >{m.device_label}</span>
            <span class="flex-1 truncate" title={m.rel_path}>{m.rel_path}</span>
            <span class="shrink-0 whitespace-nowrap text-muted-foreground">
                {formatBytes(
                    group.kind === 'folder' ? m.subtree_size : m.size
                )} · {formatTime(m.mtime)}
            </span>
            <Button
                variant="ghost"
                size="icon"
                class="size-5 shrink-0 text-muted-foreground hover:text-foreground"
                title={memberCopyLabel}
                onclick={() => copyPath(m.rel_path, app.pathSep)}>
                <Icon icon="ph:copy-fill" />
                <span class="sr-only">{memberCopyLabel}</span>
            </Button>
            <RevealButton member={m} {onreveal} />
        </li>
    {/each}
</ul>
