<!-- The top-bar name search. Searches only on Enter or the search button;
     the buttons show while the bar has focus or a search is in effect. -->
<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { search } from '$lib/stores/search.svelte';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';

    let focused = $state(false);
    let inputEl = $state<HTMLInputElement | null>(null);
    const showButtons = $derived(focused || search.active || search.busy);

    // Anything that reshapes the trees (import, delete, dedup re-run) can
    // change node ids, so re-run the applied search against the new data.
    let seenVersion = app.treeVersion;
    $effect(() => {
        const v = app.treeVersion;
        if (v === seenVersion) return;
        seenVersion = v;
        const a = search.applied;
        if (a) app.runSearch(a.query, a.caseSensitive);
    });

    function onFocusOut(e: FocusEvent) {
        // Moving focus between the input and its own buttons is not leaving.
        const next = e.relatedTarget as Node | null;
        if (!next || !(e.currentTarget as HTMLElement).contains(next)) {
            focused = false;
        }
    }

    function clear() {
        app.clearSearch();
        inputEl?.focus();
    }
</script>

<div
    class="flex h-7 w-72 min-w-0 items-center gap-0.5 border-b border-border focus-within:border-foreground/60"
    role="search"
    onfocusin={() => (focused = true)}
    onfocusout={onFocusOut}>
    <input
        bind:this={inputEl}
        bind:value={search.query}
        type="text"
        spellcheck="false"
        autocomplete="off"
        placeholder="Search names (* ? wildcards)…"
        aria-label="Search names"
        class="h-full min-w-0 flex-1 border-0 bg-transparent px-0.5 text-sm outline-none placeholder:text-muted-foreground"
        onkeydown={e => {
            if (e.key === 'Enter') app.runSearch();
        }} />
    {#if showButtons}
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 {search.caseSensitive
                ? 'text-brand'
                : 'text-muted-foreground'}"
            aria-pressed={search.caseSensitive}
            title={search.caseSensitive
                ? 'Disable case-sensitivity'
                : 'Enable case-sensitivity'}
            onclick={() => (search.caseSensitive = !search.caseSensitive)}>
            <Icon icon={search.caseSensitive ? 'ph:text-aa-bold' : 'ph:text-aa'} />
            <span class="sr-only">Match case</span>
        </Button>
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
            title="Search"
            disabled={search.busy}
            onclick={() => app.runSearch()}>
            <Icon
                icon={search.busy ? 'ph:spinner-gap-fill' : 'ph:magnifying-glass-bold'}
                class={search.busy ? 'animate-spin' : ''} />
            <span class="sr-only">Search</span>
        </Button>
        <Button
            variant="ghost"
            size="icon"
            class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
            title="Clear search"
            onclick={clear}>
            <Icon icon="ph:x-bold" />
            <span class="sr-only">Clear search</span>
        </Button>
    {/if}
</div>
