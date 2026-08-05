<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import { open } from '@tauri-apps/plugin-dialog';
    import { listen } from '@tauri-apps/api/event';
    import { hashSettingsSet } from '$lib/api';
    import type {
        DedupProgress,
        HashProgress,
        ScanConfig,
        ScanProgress,
        Workspace,
    } from '$lib/types';
    import { DUP_BADGE, DUP_BAR, dupLevel, formatBytes, pct } from '$lib/util';
    import Icon from '@iconify/svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Badge } from '$lib/components/ui/badge';
    import { Progress } from '$lib/components/ui/progress';
    import * as Empty from '$lib/components/ui/empty';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
    import { Separator } from '$lib/components/ui/separator';
    import { toast } from 'svelte-sonner';
    import ScanConfigDialog from '$lib/components/ScanConfigDialog.svelte';

    let scanDialogOpen = $state(false);
    let scanPath = $state('');
    let scanLabel = $state('');

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

    async function toggleExcluded(s: { id: number; excluded: boolean }) {
        try {
            await app.setSourceExcluded(s.id, !s.excluded);
        } catch (err) {
            toast.error(String(err));
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

    async function copyToWorkspace(
        s: { id: number; device_label: string },
        ws: Workspace
    ) {
        try {
            await app.copySourceToWorkspace(s.id, ws.id);
            toast.success(`Copied “${s.device_label}” to “${ws.name}”.`, {
                description: 'Duplicate results will need re-running there.',
            });
        } catch (err) {
            toast.error(String(err));
        }
    }

    async function onUpload(e: Event) {
        const input = e.target as HTMLInputElement;
        const files = input.files;
        if (!files || files.length === 0) return;
        const taskId = taskTray.start(
            'scan',
            files.length === 1
                ? `Importing “${files[0].name}”…`
                : `Importing ${files.length} tree files…`
        );
        try {
            for (const file of Array.from(files)) {
                const text = await file.text();
                const label = file.name.replace(/\.json$/i, '');
                await app.importJson(text, label);
            }
            taskTray.resolve(
                taskId,
                'success',
                `Imported ${files.length} tree file(s).`
            );
        } catch (err) {
            taskTray.resolve(taskId, 'error', String(err));
        } finally {
            input.value = '';
        }
    }

    async function onScan() {
        const selected = await open({ directory: true, multiple: false });
        if (!selected || typeof selected !== 'string') return;
        scanPath = selected;
        scanLabel = selected.split('/').pop() || selected;
        scanDialogOpen = true;
    }

    /** Runs after the scan-config dialog is confirmed: folder scan, then
     *  content hashing for the new source, then a fresh dedup pass -- one
     *  task-tray entry tracks all three phases in sequence. */
    async function onScanConfirm(config: ScanConfig) {
        const path = scanPath;
        const label = scanLabel;
        const taskId = taskTray.start('scan', `Scanning “${label}”…`);
        const unlistenScan = await listen<ScanProgress>('scan:progress', e => {
            taskTray.update(taskId, { phase: 'scanning', current: e.payload.current });
        });
        const unlistenHash = await listen<HashProgress>('hash:progress', e => {
            taskTray.update(taskId, {
                phase: 'hashing',
                current: e.payload.current,
                total: e.payload.total,
            });
        });
        const unlistenDedup = await listen<DedupProgress>('dedup:progress', e => {
            taskTray.update(taskId, {
                phase: e.payload.phase,
                current: e.payload.current,
                total: e.payload.total,
            });
        });
        try {
            if (!config.specLocked) {
                if (app.activeWorkspaceId == null) {
                    throw new Error('No active workspace.');
                }
                await hashSettingsSet(app.activeWorkspaceId, config.hashSpec);
            }
            const source = await app.scanFolder(
                path,
                label,
                config.hashingEnabled,
                config.hashMinSize,
                config.mediumOverride,
                config.filesystemOverride
            );
            if (source && config.hashingEnabled) {
                await app.runHashScan(source.id);
            }
            await app.runDedup();
            taskTray.resolve(taskId, 'success', `Scanned “${label}”.`);
        } catch (err) {
            taskTray.resolve(taskId, 'error', String(err));
        } finally {
            unlistenScan();
            unlistenHash();
            unlistenDedup();
        }
    }
</script>

<div class="flex min-w-0 w-full flex-row gap-3">
    <div class="flex flex-col gap-2 border-r-1 pr-3">
        <Button
            variant="outline"
            class="justify-start"
            disabled={taskTray.hasActive('scan')}
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
            disabled={taskTray.hasActive('scan')}
            onclick={onScan}>
            <Icon icon="ph:folder-open-fill" />
            <span>Scan Folder</span>
        </Button>
    </div>

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
        <div class="overflow-x-auto">
            <ul class="grid grid-flow-col gap-1.5 min-w-0">
                {#each app.sources as s (s.id)}
                    <li
                        class="group/device flex min-w-0 w-[280px] flex-col gap-1 rounded-md border bg-card px-2 py-1.5 transition-colors hover:border-brand/40 {s.excluded
                            ? 'opacity-60'
                            : ''}">
                        <div class="flex min-w-0 items-center gap-1">
                            <Icon
                                icon="ph:hard-drive-fill"
                                class="shrink-0 text-brand" />
                            {#if editingId === s.id}
                                <!-- svelte-ignore a11y_autofocus -->
                                <Input
                                    autofocus
                                    class="h-6 text-sm"
                                    bind:value={editValue}
                                    onblur={() =>
                                        commitEdit(s.id, s.device_label)}
                                    onkeydown={e => {
                                        if (e.key === 'Enter')
                                            commitEdit(s.id, s.device_label);
                                        if (e.key === 'Escape')
                                            editingId = null;
                                    }} />
                            {:else}
                                <span
                                    class="flex-1 truncate text-sm font-medium"
                                    title={s.device_label}
                                    >{s.device_label}</span>
                                <Button
                                    variant="ghost"
                                    size="icon"
                                    class="size-6 shrink-0 opacity-0 transition-opacity group-hover/device:opacity-100 {s.excluded
                                        ? 'text-muted-foreground'
                                        : ''}"
                                    aria-pressed={s.excluded}
                                    title={s.excluded
                                        ? 'Include in analysis'
                                        : 'Exclude from analysis'}
                                    onclick={() => toggleExcluded(s)}>
                                    <Icon
                                        icon={s.excluded
                                            ? 'ph:eye-slash-fill'
                                            : 'ph:eye-fill'} />
                                    <span class="sr-only"
                                        >{s.excluded
                                            ? 'Include in analysis'
                                            : 'Exclude from analysis'}</span>
                                </Button>
                                {#if app.workspaces.length > 1}
                                    <DropdownMenu.Root>
                                        <DropdownMenu.Trigger>
                                            {#snippet child({
                                                props,
                                            }: {
                                                props: Record<string, unknown>;
                                            })}
                                                <Button
                                                    {...props}
                                                    variant="ghost"
                                                    size="icon"
                                                    class="size-6 opacity-0 transition-opacity group-hover/device:opacity-100"
                                                    title="Copy to workspace">
                                                    <Icon icon="ph:copy-fill" />
                                                    <span class="sr-only"
                                                        >Copy to workspace</span>
                                                </Button>
                                            {/snippet}
                                        </DropdownMenu.Trigger>
                                        <DropdownMenu.Content align="end">
                                            <DropdownMenu.Group>
                                                <DropdownMenu.GroupHeading
                                                    >Copy to</DropdownMenu.GroupHeading>
                                                {#each app.workspaces.filter(w => w.id !== app.activeWorkspaceId) as ws (ws.id)}
                                                    <DropdownMenu.Item
                                                        onSelect={() =>
                                                            copyToWorkspace(
                                                                s,
                                                                ws
                                                            )}>
                                                        <span class="truncate"
                                                            >{ws.name}</span>
                                                    </DropdownMenu.Item>
                                                {/each}
                                            </DropdownMenu.Group>
                                        </DropdownMenu.Content>
                                    </DropdownMenu.Root>
                                {/if}
                                <Button
                                    variant="ghost"
                                    size="icon"
                                    class="size-6 opacity-0 transition-opacity group-hover/device:opacity-100"
                                    title="Rename device"
                                    onclick={() =>
                                        startEdit(s.id, s.device_label)}>
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
                            {#if s.excluded}
                                <Badge
                                    variant="outline"
                                    class="px-1.5 py-0 text-[0.65rem]">
                                    Excluded
                                </Badge>
                            {:else}
                                <Badge
                                    variant="outline"
                                    class="px-1.5 py-0 text-[0.65rem] {DUP_BADGE[
                                        dupLevel(pct(s.duplicated_pct))
                                    ]}">
                                    {pct(s.duplicated_pct)}% dup
                                </Badge>
                            {/if}
                        </div>
                    </li>
                {/each}
            </ul>
        </div>
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

<ScanConfigDialog
    bind:open={scanDialogOpen}
    path={scanPath}
    label={scanLabel}
    onConfirm={onScanConfirm} />
