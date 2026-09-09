<script lang="ts">
    import '../styles/app.css';
    import '../styles/main.css';
    import { ModeWatcher } from 'mode-watcher';
    import * as Sidebar from '$lib/components/ui/sidebar';
    import AppSidebar from '$lib/components/AppSidebar.svelte';
    import * as Breadcrumb from '$lib/components/ui/breadcrumb/index.js';
    import TaskTray from '$lib/components/TaskTray.svelte';
    import { app, viewLabel } from '$lib/stores/app.svelte';
    import { getVersion } from '@tauri-apps/api/app';
    let { children } = $props();

    // Read from the running bundle rather than hardcoded here, so this can
    // never disagree with `tauri.conf.json` -- the same file the installer and
    // the updater version against. Null until it resolves, and if it never
    // does (a `pnpm dev` frontend with no Tauri behind it) it simply doesn't
    // render.
    let version = $state<string | null>(null);
    getVersion()
        .then((v) => (version = v))
        .catch(() => (version = null));
</script>

<Sidebar.Provider class="flex flex-row">
    <AppSidebar />
    <main
        class="flex h-svh min-w-0 grow flex-col gap-2 overflow-hidden py-2 px-5 antialiased">
        <div class="flex h-9 shrink-0 flex-row items-center gap-2">
            <Breadcrumb.Root class="grow">
                <Breadcrumb.List
                    class="font-heading text-xs font-medium tracking-wide">
                    <Breadcrumb.Item>
                        <Breadcrumb.Page class="text-muted-foreground">
                            {app.activeWorkspace?.name ?? 'No workspace'}
                        </Breadcrumb.Page>
                    </Breadcrumb.Item>
                    <Breadcrumb.Separator />
                    <Breadcrumb.Item>
                        <Breadcrumb.Page>{viewLabel(app.view)}</Breadcrumb.Page>
                    </Breadcrumb.Item>
                </Breadcrumb.List>
            </Breadcrumb.Root>
            {#if version}
                <!-- `Breadcrumb.Root` above carries `grow`, so this sits hard
                     against the right edge without needing `ms-auto`. -->
                <span
                    class="shrink-0 font-heading text-xs tabular-nums text-muted-foreground">
                    v{version}
                </span>
            {/if}
        </div>
        {@render children?.()}
    </main>
</Sidebar.Provider>
<ModeWatcher defaultMode="dark" />
<TaskTray />
