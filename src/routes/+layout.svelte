<script lang="ts">
    import '../styles/app.css';
    import '../styles/main.css';
    import { ModeWatcher } from 'mode-watcher';
    import * as Sidebar from '$lib/components/ui/sidebar';
    import AppSidebar from '$lib/components/AppSidebar.svelte';
    import * as Breadcrumb from '$lib/components/ui/breadcrumb/index.js';
    import { Toaster } from '$lib/components/ui/sonner';
    import { app, viewLabel } from '$lib/stores/app.svelte';
    let { children } = $props();
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
        </div>
        {@render children?.()}
    </main>
</Sidebar.Provider>
<ModeWatcher defaultMode="dark" />
<Toaster richColors closeButton />
