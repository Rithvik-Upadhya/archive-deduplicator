<script lang="ts">
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button/index.js';
    import { cn } from '$lib/utils.js';
    import { useSidebar } from './context.svelte.js';
    import type { ComponentProps } from 'svelte';

    let {
        ref = $bindable(null),
        class: className,
        onclick,
        ...restProps
    }: ComponentProps<typeof Button> & {
        onclick?: (e: MouseEvent) => void;
    } = $props();

    const sidebar = useSidebar();
</script>

<Button
    bind:ref
    data-sidebar="trigger"
    data-slot="sidebar-trigger"
    variant="ghost"
    size="icon-lg"
    class={cn('cn-sidebar-trigger p-0', className)}
    type="button"
    onclick={e => {
        onclick?.(e);
        sidebar.toggle();
    }}
    {...restProps}>
    <Icon icon="ph:sidebar-simple-fill" />
    <span class="sr-only">Toggle Sidebar</span>
</Button>
