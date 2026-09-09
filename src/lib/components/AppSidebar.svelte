<script lang="ts">
    import * as Sidebar from '$lib/components/ui/sidebar';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import * as ButtonGroup from '$lib/components/ui/button-group';
    import * as DropdownMenu from '$lib/components/ui/dropdown-menu';
    import * as Dialog from '$lib/components/ui/dialog';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';
    import * as Field from '$lib/components/ui/field';
    import { Input } from '$lib/components/ui/input';
    import { app, VIEWS } from '$lib/stores/app.svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';
    import { open, save } from '@tauri-apps/plugin-dialog';
    import { mode, toggleMode } from 'mode-watcher';

    let dbBusy = $state(false);
    let switchingWorkspace = $state(false);
    let deleteBusy = $state(false);

    async function selectWorkspace(id: number) {
        if (switchingWorkspace || id === app.activeWorkspaceId) return;
        switchingWorkspace = true;
        try {
            await app.selectWorkspace(id);
        } catch (err) {
            taskTray.notify('Switch workspace failed', 'error', String(err));
        } finally {
            switchingWorkspace = false;
        }
    }

    /* --- Workspace create / rename dialog --- */
    let nameDialogOpen = $state(false);
    let nameDialogMode = $state<'create' | 'rename'>('create');
    let nameDialogValue = $state('');
    let nameDialogTargetId = $state<number | null>(null);

    const nameDialogTitle = $derived(
        nameDialogMode === 'create' ? 'New workspace' : 'Rename workspace'
    );

    function openCreateDialog() {
        nameDialogMode = 'create';
        nameDialogValue = `Workspace ${app.workspaces.length + 1}`;
        nameDialogTargetId = null;
        nameDialogOpen = true;
    }

    function openRenameDialog(id: number, current: string) {
        nameDialogMode = 'rename';
        nameDialogValue = current;
        nameDialogTargetId = id;
        nameDialogOpen = true;
    }

    async function submitNameDialog(e: SubmitEvent) {
        e.preventDefault();
        const name = nameDialogValue.trim();
        if (!name) return;
        nameDialogOpen = false;
        try {
            if (nameDialogMode === 'create') {
                await app.createWorkspace(name);
            } else if (nameDialogTargetId != null) {
                await app.renameWorkspace(nameDialogTargetId, name);
            }
        } catch (err) {
            taskTray.notify(
                nameDialogMode === 'create' ? 'Create failed' : 'Rename failed',
                'error',
                String(err)
            );
        }
    }

    /* --- Workspace delete confirmation --- */
    let deleteTarget = $state<{ id: number; name: string } | null>(null);

    async function confirmDelete() {
        const target = deleteTarget;
        if (!target) return;
        deleteBusy = true;
        try {
            await app.deleteWorkspace(target.id);
            deleteTarget = null;
            taskTray.notify(
                'Workspace deleted',
                'success',
                `Deleted “${target.name}”.`
            );
        } catch (err) {
            taskTray.notify('Delete failed', 'error', String(err));
        } finally {
            deleteBusy = false;
        }
    }

    /* --- Database import / export --- */
    async function exportDatabase() {
        const path = await save({
            defaultPath: 'archive-deduplicator-export.sqlite',
            filters: [
                { name: 'SQLite Database', extensions: ['sqlite', 'db'] },
            ],
        });
        if (!path) return;
        dbBusy = true;
        try {
            await app.exportDatabase(path);
            taskTray.notify(
                'Database exported',
                'success',
                'The workspace database was saved.'
            );
        } catch (err) {
            taskTray.notify('Export failed', 'error', String(err));
        } finally {
            dbBusy = false;
        }
    }

    async function importDatabase() {
        const selected = await open({
            multiple: false,
            filters: [
                { name: 'SQLite Database', extensions: ['sqlite', 'db'] },
            ],
        });
        if (!selected || typeof selected !== 'string') return;
        dbBusy = true;
        try {
            const summary = await app.importDatabase(selected);
            taskTray.notify(
                'Database imported',
                'success',
                `${summary.workspaces_added} workspace(s), ` +
                    `${summary.sources_added} source(s), ` +
                    `${summary.nodes_added} node(s).`
            );
        } catch (err) {
            taskTray.notify('Import failed', 'error', String(err));
        } finally {
            dbBusy = false;
        }
    }
</script>

<Sidebar.Root variant="floating" collapsible="icon" class="antialiased">
    <Sidebar.Header class="gap-2">
        <div
            class="flex items-center gap-1 group-data-[collapsible=icon]:flex-col">
            <Sidebar.Trigger
                class="size-8 shrink-0 text-muted-foreground hover:text-foreground" />
            <Sidebar.Menu class="min-w-0">
                <Sidebar.MenuItem>
                    <DropdownMenu.Root>
                        <DropdownMenu.Trigger>
                            {#snippet child({
                                props,
                            }: {
                                props: Record<string, unknown>;
                            })}
                                <Sidebar.MenuButton
                                    {...props}
                                    aria-disabled={switchingWorkspace}
                                    class="font-heading font-semibold tracking-tight">
                                    <Icon
                                        icon={switchingWorkspace
                                            ? 'ph:spinner-gap-fill'
                                            : 'ph:stack-fill'}
                                        class="hidden text-brand group-data-[collapsible=icon]:block {switchingWorkspace
                                            ? 'animate-spin'
                                            : ''}" />
                                    {#if switchingWorkspace}
                                        <Icon
                                            icon="ph:spinner-gap-fill"
                                            class="shrink-0 animate-spin text-brand group-data-[collapsible=icon]:hidden" />
                                    {/if}
                                    <span
                                        class="truncate opacity-100 transition-opacity delay-200 duration-200 group-data-[collapsible=icon]:opacity-0 group-data-[collapsible=icon]:delay-0">
                                        {switchingWorkspace
                                            ? 'Switching workspace…'
                                            : (app.activeWorkspace?.name ??
                                                'Select Workspace')}
                                    </span>
                                    <Icon
                                        icon="ph:caret-up-down-bold"
                                        class="ms-auto group-data-[collapsible=icon]:hidden" />
                                </Sidebar.MenuButton>
                            {/snippet}
                        </DropdownMenu.Trigger>
                        <DropdownMenu.Content
                            class="min-w-56"
                            align="start"
                            side="bottom">
                            <DropdownMenu.Group>
                                <DropdownMenu.GroupHeading
                                    >Workspaces</DropdownMenu.GroupHeading>
                                {#each app.workspaces as ws (ws.id)}
                                    <DropdownMenu.Item
                                        disabled={switchingWorkspace}
                                        onSelect={() => selectWorkspace(ws.id)}>
                                        <Icon
                                            icon={ws.id ===
                                            app.activeWorkspaceId
                                                ? 'ph:check-circle-fill'
                                                : 'ph:circle-dashed-bold'} />
                                        <span class="truncate">{ws.name}</span>
                                    </DropdownMenu.Item>
                                {/each}
                            </DropdownMenu.Group>
                            <DropdownMenu.Separator />
                            <DropdownMenu.Group>
                                <DropdownMenu.Item onSelect={openCreateDialog}>
                                    <Icon icon="ph:plus-circle-fill" />
                                    <span>New workspace</span>
                                </DropdownMenu.Item>
                                <DropdownMenu.Item
                                    disabled={!app.activeWorkspace}
                                    onSelect={() => {
                                        const ws = app.activeWorkspace;
                                        if (ws)
                                            openRenameDialog(ws.id, ws.name);
                                    }}>
                                    <Icon icon="ph:pencil-simple-fill" />
                                    <span>Rename current</span>
                                </DropdownMenu.Item>
                                <DropdownMenu.Item
                                    variant="destructive"
                                    disabled={app.workspaces.length <= 1 ||
                                        !app.activeWorkspace ||
                                        deleteBusy}
                                    onSelect={() => {
                                        const ws = app.activeWorkspace;
                                        if (ws)
                                            deleteTarget = {
                                                id: ws.id,
                                                name: ws.name,
                                            };
                                    }}>
                                    <Icon icon="ph:trash-fill" />
                                    <span>Delete current</span>
                                </DropdownMenu.Item>
                            </DropdownMenu.Group>
                        </DropdownMenu.Content>
                    </DropdownMenu.Root>
                </Sidebar.MenuItem>
            </Sidebar.Menu>
        </div>
    </Sidebar.Header>
    <Sidebar.Content>
        <Sidebar.Separator />
        <Sidebar.Group class="mt-3">
            <Sidebar.GroupLabel
                class="font-heading text-[0.7rem] font-semibold tracking-[0.12em] uppercase"
                >Tasks</Sidebar.GroupLabel>
            <Sidebar.GroupContent>
                <Sidebar.Menu class="gap-y-1">
                    {#each VIEWS as item (item.id)}
                        <Sidebar.MenuItem>
                            <Sidebar.MenuButton
                                isActive={app.view === item.id}
                                onclick={() => app.setView(item.id)}
                                tooltipContent={item.label}
                                class="group/menu-btn cursor-pointer text-sm data-[active=true]:font-medium data-[active=true]:text-sidebar-primary">
                                <Icon icon={item.icon} />
                                <span
                                    class="transition-transform duration-200 group-hover/menu-btn:translate-x-[5px]"
                                    >{item.label}</span>
                            </Sidebar.MenuButton>
                        </Sidebar.MenuItem>
                    {/each}
                </Sidebar.Menu>
            </Sidebar.GroupContent>
        </Sidebar.Group>
    </Sidebar.Content>
    <Sidebar.Footer>
        <Sidebar.Menu>
            <Sidebar.MenuItem>
                <Sidebar.MenuButton
                    onclick={toggleMode}
                    tooltipContent="Toggle theme"
                    class="cursor-pointer text-sm justify-center">
                    <Icon icon="ph:moon-fill" class="dark:hidden" />
                    <Icon icon="ph:sun-fill" class="hidden dark:block" />
                    <span class="group-data-[collapsible=icon]:hidden"
                        >{mode.current === 'dark' ? 'Light' : 'Dark'} mode</span>
                </Sidebar.MenuButton>
            </Sidebar.MenuItem>
        </Sidebar.Menu>
        <Sidebar.Separator />
        <ButtonGroup.Root
            class="w-full opacity-100 transition-opacity delay-200 duration-200 group-data-[collapsible=icon]:hidden group-data-[collapsible=icon]:opacity-0 group-data-[collapsible=icon]:delay-0">
            <Button
                variant="outline"
                class="flex-1 font-normal"
                disabled={dbBusy}
                onclick={exportDatabase}>
                <Icon icon="ph:export-fill" />
                <span>Export</span>
            </Button>
            <Button
                variant="outline"
                class="flex-1 font-normal"
                disabled={dbBusy}
                onclick={importDatabase}>
                <Icon icon="ph:git-pull-request-fill" />
                <span>Import</span>
            </Button>
        </ButtonGroup.Root>
        <Sidebar.Menu class="hidden group-data-[collapsible=icon]:flex">
            <Sidebar.MenuItem>
                <DropdownMenu.Root>
                    <DropdownMenu.Trigger>
                        {#snippet child({
                            props,
                        }: {
                            props: Record<string, unknown>;
                        })}
                            <Sidebar.MenuButton {...props}>
                                <Icon icon="ph:dots-three-outline-fill" />
                            </Sidebar.MenuButton>
                        {/snippet}
                    </DropdownMenu.Trigger>
                    <DropdownMenu.Content side="right" align="end">
                        <DropdownMenu.Group>
                            <DropdownMenu.Item
                                disabled={dbBusy}
                                onSelect={exportDatabase}>
                                <Icon icon="ph:export-fill" />
                                <span>Export</span>
                            </DropdownMenu.Item>
                            <DropdownMenu.Item
                                disabled={dbBusy}
                                onSelect={importDatabase}>
                                <Icon icon="ph:git-pull-request-fill" />
                                <span>Import</span>
                            </DropdownMenu.Item>
                        </DropdownMenu.Group>
                    </DropdownMenu.Content>
                </DropdownMenu.Root>
            </Sidebar.MenuItem>
        </Sidebar.Menu>
    </Sidebar.Footer>
</Sidebar.Root>

<Dialog.Root bind:open={nameDialogOpen}>
    <Dialog.Content class="sm:max-w-sm">
        <form onsubmit={submitNameDialog}>
            <Dialog.Header>
                <Dialog.Title>{nameDialogTitle}</Dialog.Title>
                <Dialog.Description>
                    Workspaces keep their own devices, matches and consolidation
                    plan.
                </Dialog.Description>
            </Dialog.Header>
            <Field.FieldGroup class="py-4">
                <Field.Field>
                    <Field.FieldLabel for="workspace-name"
                        >Name</Field.FieldLabel>
                    <!-- svelte-ignore a11y_autofocus -->
                    <Input
                        id="workspace-name"
                        autofocus
                        bind:value={nameDialogValue} />
                </Field.Field>
            </Field.FieldGroup>
            <Dialog.Footer>
                <Button
                    type="button"
                    variant="outline"
                    onclick={() => (nameDialogOpen = false)}>Cancel</Button>
                <Button type="submit" disabled={!nameDialogValue.trim()}>
                    {nameDialogMode === 'create' ? 'Create' : 'Save'}
                </Button>
            </Dialog.Footer>
        </form>
    </Dialog.Content>
</Dialog.Root>

<AlertDialog.Root
    open={deleteTarget !== null}
    onOpenChange={o => {
        if (!o) deleteTarget = null;
    }}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Delete this workspace?</AlertDialog.Title>
            <AlertDialog.Description>
                “{deleteTarget?.name}” and all of its devices, matches and
                consolidation plan will be permanently removed. This cannot be
                undone.
            </AlertDialog.Description>
        </AlertDialog.Header>
        <AlertDialog.Footer>
            <AlertDialog.Cancel disabled={deleteBusy}>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                disabled={deleteBusy}
                onclick={confirmDelete}
                class="bg-destructive text-white hover:bg-destructive/90">
                {#if deleteBusy}
                    <Icon icon="ph:spinner-gap-fill" class="animate-spin" />
                {/if}
                {deleteBusy ? 'Deleting…' : 'Delete'}
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
