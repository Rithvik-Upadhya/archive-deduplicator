<script lang="ts">
    import type { SourceBars } from '$lib/util';

    // The per-source colour bars of an end-state tree row. Sized to its own
    // bars, so it sits flush against whatever follows it rather than lining
    // up across rows.
    let { bars }: { bars: SourceBars } = $props();
</script>

<span class="flex h-4 shrink-0 gap-0.5" title={bars.title}>
    {#each bars.colors as color, i (i)}
        <span class="h-full w-1 rounded-[1px]" style="background: {color}"
        ></span>
    {/each}
</span>
