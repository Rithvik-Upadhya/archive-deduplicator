<script lang="ts">
    import type { SourceBars } from '$lib/util';

    // The per-source colour column of an end-state tree row. Its width comes
    // from `--col-src`, set once on the tree container by `sourceColumnVar`,
    // so the column lines up across rows however many bars each one has.
    let { bars }: { bars: SourceBars } = $props();
</script>

<span class="flex h-4 w-(--col-src) shrink-0 gap-0.5" title={bars.title}>
    {#each bars.colors as color, i (i)}
        <span class="h-full w-1 rounded-[1px]" style="background: {color}"
        ></span>
    {/each}
</span>
