<!-- The arrow after a match-group member's copy button: selects the member in
     its device tree and scrolls it into view, without switching views. -->
<script lang="ts">
    import { revealInDeviceTree } from '$lib/stores/treeExpansion.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';

    interface Props {
        member: { node_id: number; source_id: number };
        /** Called before revealing -- lets a dialog close out of the way. */
        onreveal?: () => void;
    }

    let { member, onreveal }: Props = $props();
</script>

<Button
    variant="ghost"
    size="icon"
    class="size-5 shrink-0 text-muted-foreground hover:text-foreground"
    title="Show in device tree"
    onclick={e => {
        e.stopPropagation();
        onreveal?.();
        revealInDeviceTree(member);
    }}>
    <Icon icon="ph:arrow-right" />
    <span class="sr-only">Show in device tree</span>
</Button>
