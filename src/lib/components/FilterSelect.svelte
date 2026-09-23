<!-- A multi-select list filter ("any of"), styled like the top-bar search.
     Stays open while items are toggled. The last item can't be unchecked: an
     empty filter would show nothing, and treating it as "show everything"
     would flip the list the moment the user cleared it. -->
<script lang="ts">
    import * as Select from '$lib/components/ui/select';
    import { UNDERLINE_TRIGGER } from '$lib/util';

    interface Option {
        value: string;
        label: string;
        /** Muted text beside the label. */
        hint?: string;
        /** How the trigger names this option when summarising a partial
         *  selection (e.g. `A` after a `Tiers ` prefix); defaults to `label`. */
        short?: string;
    }

    interface Props {
        ariaLabel: string;
        options: readonly Option[];
        selected: readonly string[];
        /** Trigger text while every option is selected. */
        allText: string;
        /** Trigger text prefix before the selected labels, e.g. `Tiers `. */
        prefix?: string;
        /** Called with the new selection, in option order; never empty. */
        onchange: (values: string[]) => void;
    }

    let {
        ariaLabel,
        options,
        selected,
        allText,
        prefix = '',
        onchange,
    }: Props = $props();

    const summary = $derived.by(() => {
        if (options.every(o => selected.includes(o.value))) return allText;
        const labels = options
            .filter(o => selected.includes(o.value))
            .map(o => o.short ?? o.label);
        return prefix + labels.join(', ');
    });
</script>

<!-- A function binding, not a one-way `value`: a refused change must snap the
     checkboxes back to `selected`, which a one-way prop wouldn't re-send. -->
<Select.Root
    type="multiple"
    bind:value={
        () => [...selected],
        next => {
            // Always an array with `type="multiple"`; the wrapper's prop type
            // is the single/multiple union, so narrow it rather than assert.
            const values: string[] = Array.isArray(next) ? next : [];
            if (!options.some(o => values.includes(o.value))) return;
            onchange(options.filter(o => values.includes(o.value)).map(o => o.value));
        }
    }>
    <Select.Trigger size="sm" class="w-36 {UNDERLINE_TRIGGER}" aria-label={ariaLabel}>
        <span class="truncate">{summary}</span>
    </Select.Trigger>
    <Select.Content class="min-w-56" align="start">
        {#each options as o (o.value)}
            <Select.Item value={o.value} label={o.label}>
                <span class="flex min-w-0 flex-1 items-baseline justify-between gap-3">
                    <span>{o.label}</span>
                    {#if o.hint}
                        <span class="truncate text-xs text-muted-foreground">{o.hint}</span>
                    {/if}
                </span>
            </Select.Item>
        {/each}
    </Select.Content>
</Select.Root>
