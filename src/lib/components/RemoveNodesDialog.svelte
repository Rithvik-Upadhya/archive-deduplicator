<!-- Confirmation for removing consolidated-tree nodes (and everything beneath
     them). Shared by the Consolidate and Fix Paths views. `ids` are the
     selection's roots; the dialog is open while they are non-null. -->
<script lang="ts">
    import type { NodeType } from '$lib/types';
    import * as AlertDialog from '$lib/components/ui/alert-dialog';

    interface Props {
        ids: number[] | null;
        nodeOf: (id: number) => { name: string; type: NodeType } | undefined;
        /** Extra sentence after the standard description, if any. */
        note?: string;
        onconfirm: (ids: number[]) => void;
        oncancel: () => void;
    }

    let { ids, nodeOf, note, onconfirm, oncancel }: Props = $props();

    const subject = $derived.by(() => {
        if (!ids) return '';
        if (ids.length === 1) {
            const n = nodeOf(ids[0]);
            if (!n) return '1 item';
            return `“${n.name}”${n.type === 'directory' ? ' and everything inside it' : ''}`;
        }
        return `${ids.length} items and everything inside them`;
    });
</script>

<AlertDialog.Root
    open={ids !== null}
    onOpenChange={o => {
        if (!o) oncancel();
    }}>
    <AlertDialog.Content>
        <AlertDialog.Header>
            <AlertDialog.Title>Remove from the plan?</AlertDialog.Title>
            <AlertDialog.Description>
                {subject} will be removed from the consolidated tree. Your source
                devices are not touched.{note ? ` ${note}` : ''}
            </AlertDialog.Description>
        </AlertDialog.Header>
        <AlertDialog.Footer>
            <AlertDialog.Cancel>Cancel</AlertDialog.Cancel>
            <AlertDialog.Action
                onclick={() => ids && onconfirm(ids)}
                class="bg-destructive text-white hover:bg-destructive/90">
                Remove
            </AlertDialog.Action>
        </AlertDialog.Footer>
    </AlertDialog.Content>
</AlertDialog.Root>
