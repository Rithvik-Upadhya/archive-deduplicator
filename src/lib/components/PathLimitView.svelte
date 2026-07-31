<script lang="ts">
    import * as api from '$lib/api';
    import { app } from '$lib/stores/app.svelte';
    import type { PathComponent, PathLimitEntry } from '$lib/types';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';
    import { Slider } from '$lib/components/ui/slider';
    import { Label } from '$lib/components/ui/label';
    import { ScrollArea } from '$lib/components/ui/scroll-area';
    import * as Card from '$lib/components/ui/card';
    import * as Empty from '$lib/components/ui/empty';
    import { toast } from 'svelte-sonner';

    let limit = $state(260);
    let entries = $state<PathLimitEntry[]>([]);
    let loading = $state(false);

    /** Key of the component currently being renamed: `${kind}:${node_id}`. */
    let editingKey = $state<string | null>(null);
    let editValue = $state('');

    const componentKey = (c: PathComponent) => `${c.kind}:${c.node_id}`;

    async function reload() {
        if (app.activeWorkspaceId == null) return;
        loading = true;
        try {
            entries = await api.pathfixList(app.activeWorkspaceId, limit);
        } catch (err) {
            toast.error(String(err));
        } finally {
            loading = false;
        }
    }

    let loadedFor = $state<string | null>(null);
    $effect(() => {
        const key = `${app.activeWorkspaceId}:${limit}`;
        if (app.activeWorkspaceId != null && key !== loadedFor) {
            loadedFor = key;
            reload();
        }
    });

    function startEdit(c: PathComponent) {
        editingKey = componentKey(c);
        editValue = c.name;
    }

    async function commitEdit(c: PathComponent) {
        if (app.activeWorkspaceId == null) return;
        const name = editValue.trim();
        editingKey = null;
        if (!name || name === c.name) return;
        try {
            await api.pathfixRename(
                app.activeWorkspaceId,
                c.kind,
                c.node_id,
                name
            );
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    /** Revert a virtual rename back to the original component name. */
    async function revertEdit(c: PathComponent) {
        if (app.activeWorkspaceId == null) return;
        try {
            await api.pathfixRename(
                app.activeWorkspaceId,
                c.kind,
                c.node_id,
                ''
            );
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function toggleResolved(entry: PathLimitEntry) {
        if (app.activeWorkspaceId == null) return;
        try {
            await api.pathfixSetResolved(
                app.activeWorkspaceId,
                entry.leaf_kind,
                entry.node_id,
                !entry.resolved
            );
            await reload();
        } catch (err) {
            toast.error(String(err));
        }
    }

    const overCount = $derived(
        entries.filter(e => e.length > limit && !e.resolved).length
    );
</script>

<div class="flex min-h-0 grow flex-col gap-3 overflow-hidden">
    <div class="flex flex-wrap items-end gap-6">
        <div class="flex min-w-56 flex-col gap-1.5">
            <Label for="limit" class="text-xs">
                Path length limit: <strong>{limit}</strong>
            </Label>
            <Slider
                id="limit"
                type="single"
                min={80}
                max={400}
                step={10}
                bind:value={limit} />
        </div>
        <div class="flex items-baseline gap-2 text-sm text-muted-foreground">
            <span
                class="text-2xl font-bold"
                class:text-destructive={overCount > 0}
                class:text-ok={overCount === 0}>{overCount}</span>
            <span>branches over limit</span>
        </div>
        <Button variant="outline" disabled={loading} onclick={reload}>
            <Icon
                icon={loading ? 'ph:spinner-gap-fill' : 'ph:radar-fill'}
                class={loading ? 'animate-spin' : ''} />
            <span>{loading ? 'Scanning…' : 'Rescan'}</span>
        </Button>
    </div>

    {#if entries.length === 0}
        <Empty.Root class="border border-dashed">
            <Empty.Header>
                <Empty.Media variant="icon">
                    <Icon
                        icon={loading
                            ? 'ph:spinner-gap-fill'
                            : 'ph:check-circle-fill'}
                        class={loading ? 'animate-spin' : ''} />
                </Empty.Media>
                <Empty.Title>
                    {loading ? 'Scanning…' : 'Nothing over the limit'}
                </Empty.Title>
                <Empty.Description>
                    {loading
                        ? 'Measuring the consolidated end-state tree.'
                        : 'No paths exceed the current limit.'}
                </Empty.Description>
            </Empty.Header>
        </Empty.Root>
    {:else}
        <ScrollArea class="min-h-0 grow">
            <ul class="flex flex-col gap-2 pe-2">
                {#each entries as entry (`${entry.leaf_kind}:${entry.node_id}`)}
                    <li
                        class="rounded-md border border-destructive/50 bg-destructive/[0.04] data-[ok=true]:border-border data-[ok=true]:bg-transparent data-[ok=true]:opacity-70"
                        data-ok={entry.resolved || entry.length <= limit}>
                        <Card.Root class="gap-2 border-0 py-2 shadow-none">
                            <Card.Content class="flex flex-col gap-1.5 px-3">
                                <div class="flex items-center gap-2">
                                    <Badge
                                        variant={entry.length <= limit
                                            ? 'secondary'
                                            : 'destructive'}
                                        class="px-1.5 py-0 text-[0.7rem]">
                                        {entry.length} chars
                                    </Badge>
                                    <span
                                        class="text-xs text-muted-foreground capitalize">
                                        {entry.leaf_kind === 'cons'
                                            ? 'consolidated'
                                            : 'source'}
                                    </span>
                                    <Button
                                        variant="ghost"
                                        size="sm"
                                        class="ms-auto h-6 gap-1 text-xs"
                                        title="Mark this branch resolved"
                                        onclick={() => toggleResolved(entry)}>
                                        <Icon
                                            icon={entry.resolved
                                                ? 'ph:check-circle-fill'
                                                : 'ph:circle-dashed-bold'}
                                            class={entry.resolved
                                                ? 'text-ok'
                                                : ''} />
                                        {entry.resolved
                                            ? 'Resolved'
                                            : 'Mark resolved'}
                                    </Button>
                                </div>

                                <code
                                    class="block break-all text-[0.7rem] text-muted-foreground">
                                    {entry.effective_path}
                                </code>

                                <!-- Editable path components -->
                                <div class="flex flex-wrap items-center gap-1">
                                    {#each entry.components as c, i (componentKey(c))}
                                        {#if i > 0}
                                            <span
                                                class="text-xs text-muted-foreground"
                                                >\</span>
                                        {/if}
                                        {#if editingKey === componentKey(c)}
                                            <!-- svelte-ignore a11y_autofocus -->
                                            <Input
                                                autofocus
                                                class="h-6 w-36 text-xs"
                                                bind:value={editValue}
                                                onblur={() => commitEdit(c)}
                                                onkeydown={e => {
                                                    if (e.key === 'Enter')
                                                        commitEdit(c);
                                                    if (e.key === 'Escape')
                                                        editingKey = null;
                                                }} />
                                        {:else}
                                            <button
                                                type="button"
                                                class="inline-flex items-center gap-1 rounded-sm border px-1.5 py-0.5 text-xs hover:border-brand hover:text-brand data-[edited=true]:border-brand data-[edited=true]:text-brand"
                                                data-edited={c.edited}
                                                title={c.edited
                                                    ? `Originally “${c.original_name}” — click to rename`
                                                    : 'Click to rename'}
                                                onclick={() => startEdit(c)}>
                                                <Icon
                                                    icon={c.type === 'directory'
                                                        ? 'ph:folder-fill'
                                                        : 'ph:file-fill'} />
                                                {c.name}
                                            </button>
                                            {#if c.edited}
                                                <Button
                                                    variant="ghost"
                                                    size="icon"
                                                    class="size-5"
                                                    title="Revert to original name"
                                                    onclick={() =>
                                                        revertEdit(c)}>
                                                    <Icon
                                                        icon="ph:arrow-counter-clockwise-bold" />
                                                    <span class="sr-only"
                                                        >Revert</span>
                                                </Button>
                                            {/if}
                                        {/if}
                                    {/each}
                                </div>
                            </Card.Content>
                        </Card.Root>
                    </li>
                {/each}
            </ul>
        </ScrollArea>
    {/if}
</div>
