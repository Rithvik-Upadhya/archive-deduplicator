<!-- Name prompt for a new root folder in the consolidated tree. Shared by the
     Consolidate and Fix Paths views so the two cannot drift; each supplies
     its own `oncreate`, since they refresh their trees differently. -->
<script lang="ts">
    import { untrack } from 'svelte';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import * as Dialog from '$lib/components/ui/dialog';
    import * as Field from '$lib/components/ui/field';

    interface Props {
        open: boolean;
        /** Called with the trimmed, non-empty name. */
        oncreate: (name: string) => void;
    }

    let { open = $bindable(), oncreate }: Props = $props();

    let name = $state('New Folder');

    // Fresh default each time the dialog opens, not whatever was typed (or
    // left) last time.
    $effect(() => {
        if (open) untrack(() => (name = 'New Folder'));
    });

    function submit(e: SubmitEvent) {
        e.preventDefault();
        const trimmed = name.trim();
        if (!trimmed) return;
        open = false;
        oncreate(trimmed);
    }
</script>

<Dialog.Root bind:open>
    <Dialog.Content class="sm:max-w-sm">
        <form onsubmit={submit}>
            <Dialog.Header>
                <Dialog.Title>New folder</Dialog.Title>
                <Dialog.Description>
                    Adds an empty folder at the root of the consolidated tree.
                </Dialog.Description>
            </Dialog.Header>
            <Field.FieldGroup class="py-4">
                <Field.Field>
                    <Field.FieldLabel for="folder-name">Name</Field.FieldLabel>
                    <!-- svelte-ignore a11y_autofocus -->
                    <Input id="folder-name" autofocus bind:value={name} />
                </Field.Field>
            </Field.FieldGroup>
            <Dialog.Footer>
                <Button
                    type="button"
                    variant="outline"
                    onclick={() => (open = false)}>Cancel</Button>
                <Button type="submit" disabled={!name.trim()}>Create</Button>
            </Dialog.Footer>
        </form>
    </Dialog.Content>
</Dialog.Root>
