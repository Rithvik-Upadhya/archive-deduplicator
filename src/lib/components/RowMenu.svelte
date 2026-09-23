<!-- The ⋮ menu that ends every row of the Consolidate and Fix Paths trees,
     shared so the two cannot drift. "Copy source path" yields the row's path
     on its device (`origin_path`, source-relative like the device tree's own
     copy); "Copy new path" yields its end-state path. Neither carries a device
     name. -->
<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { copyPath } from '$lib/util';
    import Icon from '$lib/components/Icon.svelte';
    import { Button } from '$lib/components/ui/button';
    import * as DropdownMenu from '$lib/components/ui/dropdown-menu';

    interface Props {
        /** The row's path on its source device; null for a hand-made folder. */
        sourcePath: string | null;
        /** The row's end-state path, root-first. */
        newPath: () => string[];
        /** Whether the row itself was renamed (offers a reset). */
        renamed: boolean;
        /** Folders also offer a new subfolder. */
        isDir: boolean;
        onrename: () => void;
        onreset: () => void;
        onnewfolder: () => void;
    }

    let {
        sourcePath,
        newPath,
        renamed,
        isDir,
        onrename,
        onreset,
        onnewfolder,
    }: Props = $props();
</script>

<!-- Opening the menu must not select the row: the row selects on click and
     on Enter. -->
<span
    class="inline-flex shrink-0"
    role="presentation"
    onclick={e => e.stopPropagation()}
    onkeydown={e => e.stopPropagation()}>
    <DropdownMenu.Root>
        <DropdownMenu.Trigger>
            {#snippet child({ props }: { props: Record<string, unknown> })}
                <Button
                    {...props}
                    variant="ghost"
                    size="icon"
                    class="size-6 shrink-0 text-muted-foreground hover:text-foreground"
                    title="Actions"
                    aria-label="Actions">
                    <Icon icon="ph:dots-three-vertical-bold" />
                </Button>
            {/snippet}
        </DropdownMenu.Trigger>
        <!-- No focus return on close: Rename mounts an autofocused input, and
             refocusing the trigger would blur it and commit at once. -->
        <DropdownMenu.Content
            align="end"
            class="w-max"
            onCloseAutoFocus={e => e.preventDefault()}>
            <DropdownMenu.Item
                disabled={sourcePath == null}
                onSelect={() => sourcePath && copyPath(sourcePath, app.pathSep)}>
                <Icon icon="ph:hard-drive-fill" />
                <span>Copy source path</span>
            </DropdownMenu.Item>
            <DropdownMenu.Item
                onSelect={() => copyPath(newPath(), app.pathSep)}>
                <Icon icon="ph:copy-fill" />
                <span>Copy new path</span>
            </DropdownMenu.Item>
            <DropdownMenu.Separator />
            <DropdownMenu.Item onSelect={onrename}>
                <Icon icon="ph:pencil-simple-fill" />
                <span>Rename</span>
            </DropdownMenu.Item>
            {#if renamed}
                <DropdownMenu.Item onSelect={onreset}>
                    <Icon icon="ph:arrow-counter-clockwise-bold" />
                    <span>Reset name</span>
                </DropdownMenu.Item>
            {/if}
            {#if isDir}
                <DropdownMenu.Item onSelect={onnewfolder}>
                    <Icon icon="ph:folder-plus-fill" />
                    <span>New subfolder</span>
                </DropdownMenu.Item>
            {/if}
        </DropdownMenu.Content>
    </DropdownMenu.Root>
</span>
