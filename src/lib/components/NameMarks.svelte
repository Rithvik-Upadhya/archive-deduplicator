<!-- Indicators that sit right after a row's name in the Consolidate and Fix
     Paths trees, shared so the two cannot drift: a reset button when the row
     itself was renamed, a textbox mark when something beneath it was, and a
     strikethrough mark when something beneath it is to be deleted. -->
<script lang="ts">
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';

    interface Props {
        /** The row's name before it was renamed; null when not renamed. */
        original: string | null;
        /** Renamed items anywhere beneath the row. */
        renamedBelow: number;
        /** Whether something beneath the row is struck while the row is not. */
        struckBelow: boolean;
        onreset?: () => void;
    }

    let { original, renamedBelow, struckBelow, onreset }: Props = $props();

    const resetLabel = $derived(`Reset to “${original}”`);
    const renamedLabel = $derived(
        `${renamedBelow} renamed ${renamedBelow === 1 ? 'item' : 'items'} inside`
    );
</script>

{#if original != null}
    <Button
        variant="ghost"
        size="icon"
        class="size-5 shrink-0 text-muted-foreground hover:text-foreground"
        title={resetLabel}
        aria-label={resetLabel}
        onclick={e => {
            e.stopPropagation();
            onreset?.();
        }}>
        <Icon icon="ph:arrow-counter-clockwise-bold" class="size-3" />
    </Button>
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
