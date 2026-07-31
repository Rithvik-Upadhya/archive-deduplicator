<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { Skeleton } from '$lib/components/ui/skeleton';
    import DedupView from '$lib/components/DedupView.svelte';
    import ConsolidationView from '$lib/components/ConsolidationView.svelte';
    import PathLimitView from '$lib/components/PathLimitView.svelte';

    // Boot the store once the shell is mounted. `init()` is idempotent.
    $effect(() => {
        app.init();
    });
</script>

<section class="flex min-h-0 grow flex-col overflow-hidden pt-2">
    {#if app.loading}
        <div class="flex flex-col gap-4 p-4">
            <Skeleton class="h-24 w-full" />
            <div class="grid grid-cols-2 gap-4">
                <Skeleton class="h-64 w-full" />
                <Skeleton class="h-64 w-full" />
            </div>
        </div>
    {:else if app.view === 'dedup'}
        <DedupView />
    {:else if app.view === 'consolidate'}
        <ConsolidationView />
    {:else if app.view === 'pathlimits'}
        <PathLimitView />
    {/if}
</section>
