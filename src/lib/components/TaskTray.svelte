<script lang="ts">
    import { taskTray } from '$lib/stores/tasks.svelte';
    import { TASK_STATUS_BAR, TASK_STATUS_ICON } from '$lib/util';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Progress } from '$lib/components/ui/progress';
    import * as Card from '$lib/components/ui/card';

    const PHASE_LABELS: Record<string, string> = {
        loading: 'Loading files…',
        matching: 'Comparing files…',
        grouping: 'Recording matches…',
        folder_rollup: 'Clustering folders…',
        annotating: 'Updating tree annotations…',
        done: 'Finalizing…',
    };
</script>

{#if taskTray.tasks.length > 0}
    <!-- Offset above the sonner toaster's default bottom-right corner so a
         transient toast (rename/delete/copy confirmations) doesn't overlap a
         standing task card. -->
    <div class="fixed right-4 bottom-4 z-40 flex w-80 flex-col-reverse gap-2">
        {#each taskTray.tasks as task (task.id)}
            <Card.Root class="gap-2 py-3 shadow-lg">
                <Card.Content class="flex flex-col gap-2 px-3">
                    <div class="flex items-center gap-2">
                        <Icon
                            icon={TASK_STATUS_ICON[task.status].icon}
                            class="shrink-0 {TASK_STATUS_ICON[task.status]
                                .class}" />
                        <span class="flex-1 truncate text-sm font-medium"
                            >{task.label}</span>
                        {#if task.status !== 'running'}
                            <Button
                                variant="ghost"
                                size="icon"
                                class="size-6 shrink-0"
                                title="Dismiss"
                                onclick={() => taskTray.dismiss(task.id)}>
                                <Icon icon="ph:x-bold" />
                                <span class="sr-only">Dismiss</span>
                            </Button>
                        {/if}
                    </div>
                    {#if task.status === 'running'}
                        {#if task.kind === 'dedup' && task.total > 0}
                            <Progress
                                value={task.current}
                                max={task.total}
                                class="h-1 {TASK_STATUS_BAR[task.status]}" />
                            <span class="text-[0.7rem] text-muted-foreground">
                                {PHASE_LABELS[task.phase ?? ''] ??
                                    task.phase ??
                                    '…'}
                            </span>
                        {:else if task.kind === 'scan'}
                            <span class="text-[0.7rem] text-muted-foreground">
                                {task.current.toLocaleString()} files scanned…
                            </span>
                        {/if}
                    {:else if task.message}
                        <span class="text-[0.7rem] text-muted-foreground"
                            >{task.message}</span>
                    {/if}
                </Card.Content>
            </Card.Root>
        {/each}
    </div>
{/if}
