<!-- Indicators that sit right after a row's name in the Consolidate and Fix
     Paths trees, shared so the two cannot drift: a textbox mark in the source's
     colour when the row itself was renamed (reset is in `RowMenu`), a muted
     textbox mark when something beneath it was, a
     strikethrough mark when something beneath it is to be deleted, and an
     arrows-out mark for a move within one source -- in the source's colour on
     the moved row, muted on every folder above it (see `util.relocationsFor`). -->
<script lang="ts">
    import Icon from '$lib/components/Icon.svelte';

    interface Props {
        /** The row's name before it was renamed; null when not renamed. */
        original: string | null;
        /** The row's own source colour, for its renamed mark -- the same
         *  colour the relocation mark uses. */
        renamedColor: string;
        /** Renamed items anywhere beneath the row. */
        renamedBelow: number;
        /** Whether something beneath the row is struck while the row is not. */
        struckBelow: boolean;
        /** Source colour and tooltip when the row itself was moved within its
         *  own source; null otherwise. */
        relocatedColor?: string | null;
        relocatedTitle?: string | null;
        /** Moves within a source anywhere beneath the row. */
        movedBelow?: number;
    }

    let {
        original,
        renamedColor,
        renamedBelow,
        struckBelow,
        relocatedColor = null,
        relocatedTitle = null,
        movedBelow = 0,
    }: Props = $props();

    const renamedTitle = $derived(`Renamed — originally “${original}”`);
    const renamedLabel = $derived(
        `${renamedBelow} renamed ${renamedBelow === 1 ? 'item' : 'items'} inside`
    );
    const movedLabel = $derived(
        `${movedBelow} ${movedBelow === 1 ? 'item' : 'items'} inside moved within ${movedBelow === 1 ? 'its' : 'their'} own device`
    );
</script>

{#if original != null}
    <span
        class="inline-flex shrink-0"
        style="color: {renamedColor}"
        title={renamedTitle}
        aria-label={renamedTitle}>
        <Icon icon="ph:textbox" class="size-3.5" />
    </span>
{/if}
{#if renamedBelow > 0}
    <span
        class="inline-flex shrink-0 text-muted-foreground"
        title={renamedLabel}
        aria-label={renamedLabel}>
        <Icon icon="ph:textbox" class="size-3.5" />
    </span>
{/if}
{#if struckBelow}
    <span
        class="inline-flex shrink-0 text-muted-foreground"
        title="Some items inside are marked for deletion"
        aria-label="Some items inside are marked for deletion">
        <Icon icon="ph:text-strikethrough" class="size-3.5" />
    </span>
{/if}
{#if relocatedColor != null}
    <span
        class="inline-flex shrink-0"
        style="color: {relocatedColor}"
        title={relocatedTitle}
        aria-label={relocatedTitle}>
        <Icon icon="ph:arrows-out-cardinal" class="size-3.5" />
    </span>
{/if}
{#if movedBelow > 0}
    <span
        class="inline-flex shrink-0 text-muted-foreground"
        title={movedLabel}
        aria-label={movedLabel}>
        <Icon icon="ph:arrows-out-cardinal" class="size-3.5" />
    </span>
{/if}
