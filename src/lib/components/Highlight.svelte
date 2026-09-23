<!-- A name with the applied search's matches marked. Reads `search.applied`
     itself, so a caller just swaps `{name}` for `<Highlight text={name} />`.
     The mark keeps the row's own text colour (`--hit` is chosen to carry
     both themes' foreground), so struck/done tints survive a search. -->
<script lang="ts">
    import { search } from '$lib/stores/search.svelte';

    let { text }: { text: string } = $props();

    const parts = $derived(search.compiled?.parts(text) ?? [{ text, hit: false }]);
</script>

{#each parts as p, i (i)}{#if p.hit}<mark class="rounded-[2px] bg-hit text-inherit"
            >{p.text}</mark>{:else}{p.text}{/if}{/each}
