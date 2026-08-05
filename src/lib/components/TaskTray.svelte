<script lang="ts">
    import { taskTray, type Task } from '$lib/stores/tasks.svelte';
    import { TASK_STATUS_BAR, TASK_STATUS_ICON } from '$lib/util';
    import { cancelHashScan } from '$lib/api';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Progress } from '$lib/components/ui/progress';
    import * as Card from '$lib/components/ui/card';
    import { toast } from 'svelte-sonner';

    const PHASE_LABELS: Record<string, string> = {
        loading: 'Loading files…',
        matching: 'Comparing files…',
        grouping: 'Recording matches…',
        folder_rollup: 'Clustering folders…',
        annotating: 'Updating tree annotations…',
        done: 'Finalizing…',
    };

    async function onCancel(task: Task) {
        if (task.sourceId == null) return;
        taskTray.update(task.id, { cancelling: true });
        try {
            const ok = await cancelHashScan(task.sourceId);
            // `false` means nothing was actually found running for this
            // source (already finished, or the flag beat the registry
            // insert) -- re-enable the button rather than leaving it stuck
            // on "Cancelling…" forever waiting for a stop that will never
            // happen.
            if (!ok) taskTray.update(task.id, { cancelling: false });
        } catch (err) {
            toast.error(String(err));
            taskTray.update(task.id, { cancelling: false });
        }
    }
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
                        {#if task.status === 'running' && task.phase === 'hashing' && task.sourceId != null}
                            <Button
                                variant="ghost"
                                size="sm"
                                class="h-6 shrink-0 px-1.5 text-[0.7rem]"
                                disabled={task.cancelling}
                                onclick={() => onCancel(task)}>
                                {task.cancelling ? 'Cancelling…' : 'Cancel'}
                            </Button>
                        {/if}
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
                                {#if task.phase === 'hashing'}
                                    {#if task.total > 0}
                                        {task.current.toLocaleString()} / {task.total.toLocaleString()}
                                        files hashed…
                                    {:else}
                                        Hashing…
                                    {/if}
                                {:else}
                                    {task.current.toLocaleString()} files scanned…
                                {/if}
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
