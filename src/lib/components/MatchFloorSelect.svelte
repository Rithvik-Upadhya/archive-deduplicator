<!-- Sets the min-confidence floor by match type rather than by number: each
     type is explained in plain words, and choosing one keeps it and every
     stricter type. The options come from `MATCH_FLOORS`. -->
<script lang="ts">
    import * as Select from '$lib/components/ui/select';
    import { MATCH_FLOORS, UNDERLINE_TRIGGER, matchTier } from '$lib/util';

    interface Props {
        id: string;
        /** The current floor; one of `MATCH_FLOORS`' confidences. */
        value: number;
        /** Called with the new floor, only when it differs from `value`. */
        oncommit: (value: number) => void;
    }

    let { id, value, oncommit }: Props = $props();

    const current = $derived(MATCH_FLOORS.find(f => f.confidence === value));
</script>

<Select.Root
    type="single"
    value={String(value)}
    onValueChange={v => {
        const next = Number(v);
        if (next !== value) oncommit(next);
    }}>
    <Select.Trigger {id} size="sm" class="w-72 {UNDERLINE_TRIGGER}">
        {current?.label ?? matchTier(value).label}
    </Select.Trigger>
    <Select.Content class="w-96" align="start">
        {#each MATCH_FLOORS as floor (floor.confidence)}
            <Select.Item value={String(floor.confidence)} label={floor.label}>
                <div class="flex min-w-0 flex-1 flex-col gap-0.5 py-1 whitespace-normal">
                    <span class="flex items-baseline justify-between gap-2">
                        <span class="font-medium">{floor.label}</span>
                        <span class="text-xs text-muted-foreground"
                            >Tier {floor.tier}</span>
                    </span>
                    <span class="text-xs text-muted-foreground">{floor.description}</span>
                </div>
            </Select.Item>
        {/each}
    </Select.Content>
</Select.Root>
