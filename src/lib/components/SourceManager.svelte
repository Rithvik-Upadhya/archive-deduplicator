<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { open } from '@tauri-apps/plugin-dialog';
    import { DUP_BADGE, DUP_BAR, dupLevel, formatBytes, pct } from '$lib/util';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';
    import { Progress } from '$lib/components/ui/progress';
    import * as Empty from '$lib/components/ui/empty';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import { Separator } from '$lib/components/ui/separator';
    import { toast } from 'svelte-sonner';

    let busy = $state(false);
    let editingId = $state<number | null>(null);
    let editValue = $state('');
    let deleteTarget = $state<{ id: number; label: string } | null>(null);
    let fileInput = $state<HTMLInputElement | null>(null);

    function startEdit(id: number, current: string) {
        editingId = id;
        editValue = current;
    }

    async function commitEdit(id: number, current: string) {
        const name = editValue.trim();
        editingId = null;
        if (name && name !== current) {
            try {
                await app.renameDevice(id, name);
            } catch (err) {
                toast.error(String(err));
            }
        }
    }

    async function confirmDelete() {
        const target = deleteTarget;
        deleteTarget = null;
        if (!target) return;
        try {
            await app.deleteSource(target.id);
            toast.success(`Removed “${target.label}”.`);
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function onUpload(e: Event) {
        const input = e.target as HTMLInputElement;
        const files = input.files;
        if (!files || files.length === 0) return;
        busy = true;
        try {
            for (const file of Array.from(files)) {
                const text = await file.text();
                const label = file.name.replace(/\.json$/i, '');
                await app.importJson(text, label);
            }
            toast.success(`Imported ${files.length} tree file(s).`);
        } catch (err) {
            toast.error(String(err));
        } finally {
            busy = false;
            input.value = '';
        }
    }

    async function onScan() {
        const selected = await open({ directory: true, multiple: false });
        if (!selected || typeof selected !== 'string') return;
        const label = selected.split('/').pop() || selected;
        busy = true;
        try {
            await app.scanFolder(selected, label);
            toast.success(`Scanned “${label}”.`);
        } catch (err) {
            toast.error(String(err));
        } finally {
            busy = false;
        }
    }
</script>

<div class="flex flex-row gap-3">
    <div class="flex flex-col gap-2 border-r-1 pr-3">
        <Button
            variant="outline"
            class="justify-start"
            disabled={busy}
            onclick={() => fileInput?.click()}>
            <Icon icon="ph:file-plus-fill" />
            <span>Import Tree File</span>
        </Button>
        <input
            bind:this={fileInput}
            type="file"
            accept=".json,application/json"
            multiple
            class="sr-only"
            onchange={onUpload} />

        <Button
            variant="outline"
            class="justify-start"
            disabled={busy}
            onclick={onScan}>
            <Icon icon="ph:folder-open-fill" />
            <span>Scan Folder</span>
        </Button>
    </div>

    {#if busy}
        <p
            class="flex items-center gap-2 text-xs text-muted-foreground"
            aria-live="polite"
            style="writing-mode: sideways-lr;">
            <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
            Working…
        </p>
    {/if}

    <h2 class="section-label text-center" style="writing-mode: sideways-lr;">
        Devices
    </h2>

    {#if app.sources.length === 0}
        <Empty.Root class="border border-dashed py-6">
            <Empty.Header>
                <Empty.Media variant="icon">
                    <Icon icon="ph:hard-drives-fill" />
                </Empty.Media>
                <Empty.Title>No devices yet</Empty.Title>
                <Empty.Description>
                    Import a tree JSON or scan a folder to begin.
                </Empty.Description>
            </Empty.Header>
        </Empty.Root>
    {:else}
        <ul class="flex flex-row gap-1.5 grow overflow-x-auto">
            {#each app.sources as s (s.id)}
                <li
                    class="group/device flex flex-col gap-1 rounded-md border bg-card px-2 py-1.5 transition-colors hover:border-brand/40">
                    <div class="flex min-w-0 items-center gap-1">
                        <Icon
                            icon="ph:hard-drive-fill"
                            class="shrink-0 text-brand" />
                        {#if editingId === s.id}
                            <!-- svelte-ignore a11y_autofocus -->
                            <Input
                                autofocus
                                class="h-7 text-sm"
                                bind:value={editValue}
                                onblur={() => commitEdit(s.id, s.device_label)}
                                onkeydown={e => {
                                    if (e.key === 'Enter')
                                        commitEdit(s.id, s.device_label);
                                    if (e.key === 'Escape') editingId = null;
                                }} />
                        {:else}
                            <span
                                class="flex-1 truncate text-sm font-medium"
                                title={s.device_label}>{s.device_label}</span>
                            <Button
                                variant="ghost"
                                size="icon"
                                class="size-6 opacity-0 transition-opacity group-hover/device:opacity-100"
                                title="Rename device"
                                onclick={() => startEdit(s.id, s.device_label)}>
                                <Icon icon="ph:pencil-simple-fill" />
                                <span class="sr-only">Rename device</span>
                            </Button>
                            <Button
                                variant="ghost"
                                size="icon"
                                class="size-6 text-muted-foreground opacity-0 transition-opacity group-hover/device:opacity-100 hover:text-destructive"
                                title="Remove device"
                                onclick={() =>
                                    (deleteTarget = {
                                        id: s.id,
                                        label: s.device_label,
                                    })}>
                                <Icon icon="ph:trash-fill" />
                                <span class="sr-only">Remove device</span>
                            </Button>
                        {/if}
                    </div>
                    <Progress
                        value={pct(s.duplicated_pct)}
                        class="h-1 {DUP_BAR[
                            dupLevel(pct(s.duplicated_pct))
                        ]}" />
                    <div
                        class="flex items-center justify-between font-heading text-[0.7rem] tabular-nums text-muted-foreground">
                        <span>{formatBytes(s.total_size)}</span>
                        <Badge
                            variant="outline"
                            class="px-1.5 py-0 text-[0.65rem] {DUP_BADGE[
                                dupLevel(pct(s.duplicated_pct))
                            ]}">
                            {pct(s.duplicated_pct)}% dup
                        </Badge>
                    </div>
                </li>
            {/each}
        </ul>
    {/if}
</div>

<AlertDialog.Root
    open={deleteTarget !== null}
    onOpenChange={o => {
        if (!o) deleteTarget = null;
    }}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Remove this device?</AlertDialog.Title>
            <AlertDialog.Description>
                “{deleteTarget?.label}” and its scanned tree will be removed
                from this workspace. Duplicate results will need re-running.
            </AlertDialog.Description>
        </AlertDialog.Header>
        <AlertDialog.Footer>
            <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                onclick={confirmDelete}
                class="bg-destructive text-white hover:bg-destructive/90">
                Remove
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
