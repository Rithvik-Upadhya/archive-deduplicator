<!-- Closes every open folder in a tree pane by emptying its saved expansion.
     Shared by the device-tree list, the consolidated tree and Fix Paths, so
     the three look and behave alike. Pass the set the pane is reading now --
     the search-scoped one while searching. -->
<script lang="ts">
    import type { ToggleSet } from '$lib/stores/treeExpansion.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';

    let { set }: { set: ToggleSet } = $props();
</script>

<Button
    variant="outline"
    size="sm"
    title="Collapse all folders"
    aria-label="Collapse all folders"
    onclick={() => set.clear()}>
    <Icon icon="ph:arrows-in-line-vertical" />
</Button>
