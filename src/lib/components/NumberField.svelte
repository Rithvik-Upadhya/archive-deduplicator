<!-- A whole-number field styled like the top-bar search: no background, a
     bottom border, and a muted unit suffix. A value takes effect only on
     Enter or blur; anything that is not a whole number (empty included)
     reverts to the last committed value, and in-range clamping is shown in
     the field rather than silently applied. -->
<script lang="ts">
    interface Props {
        id: string;
        value: number;
        /** Unit shown after the number, e.g. `KB` or `%`. */
        suffix: string;
        min?: number;
        max?: number;
        /** Called with the new value, only when it differs from `value`. */
        oncommit: (value: number) => void;
    }

    let { id, value, suffix, min = 0, max, oncommit }: Props = $props();

    let text = $state('');
    let focused = $state(false);

    // Follow `value` (initial load, workspace switch) except mid-edit.
    $effect(() => {
        const v = value;
        if (!focused) text = String(v);
    });

    function commit() {
        const raw = text.trim();
        if (!/^\d+$/.test(raw)) {
            text = String(value);
            return;
        }
        let next = Number(raw);
        next = Math.max(min, next);
        if (max !== undefined) next = Math.min(max, next);
        text = String(next);
        if (next !== value) oncommit(next);
    }
</script>

<div
    class="flex h-7 w-28 items-center gap-1 border-b border-border focus-within:border-foreground/60">
    <input
        {id}
        type="text"
        inputmode="numeric"
        autocomplete="off"
        spellcheck="false"
        class="h-full min-w-0 flex-1 border-0 bg-transparent px-0.5 text-sm tabular-nums outline-none"
        bind:value={text}
        onfocus={() => (focused = true)}
        onblur={() => {
            commit();
            focused = false;
        }}
        onkeydown={e => {
            // Blurring is what commits, so Enter and Escape share one path.
            if (e.key === 'Enter') e.currentTarget.blur();
            if (e.key === 'Escape') {
                text = String(value);
                e.currentTarget.blur();
            }
        }} />
    <span class="shrink-0 text-xs text-muted-foreground">{suffix}</span>
</div>
