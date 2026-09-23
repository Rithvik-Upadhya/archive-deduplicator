<!-- A match-group member's path with the search highlight on its name only:
     the search matches names, so marking a hit in a parent folder would claim
     a match the filter never made. -->
<script lang="ts">
    import Highlight from './Highlight.svelte';

    let { relPath, name }: { relPath: string; name: string } = $props();

    const endsInName = $derived(name !== '' && relPath.endsWith(name));
    const prefix = $derived(endsInName ? relPath.slice(0, -name.length) : '');
</script>

{prefix}<Highlight text={endsInName ? name : relPath} />
