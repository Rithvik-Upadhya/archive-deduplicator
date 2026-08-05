<script lang="ts">
    import { getSizeBuckets, previewHashThreshold } from '$lib/api';
    import type { ScanPreview, SizeBucket } from '$lib/types';
    import { formatBytes } from '$lib/util';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Label } from '$lib/components/ui/label';
    import * as Dialog from '$lib/components/ui/dialog';
    import Icon from '@iconify/svelte';

    let {
        open = $bindable(false),
        sourceId,
        label,
        initialHashMinSize,
        onConfirm,
    }: {
        open: boolean;
        sourceId: number;
        label: string;
        initialHashMinSize: number;
        onConfirm: (hashMinSize: number) => void;
    } = $props();

    let loadingBuckets = $state(true);
    let buckets = $state<SizeBucket[]>([]);
    let thresholdKb = $state(0);
    let preview = $state<ScanPreview | null>(null);
    let previewTimer: ReturnType<typeof setTimeout> | undefined;

    async function refreshPreview() {
        try {
            preview = await previewHashThreshold(sourceId, thresholdKb * 1024);
        } catch {
            // A stale/cancelled preview fetch is not worth surfacing --
            // the next input change (or the dialog reopening) retries.
        }
    }

    function thresholdChanged() {
        clearTimeout(previewTimer);
        previewTimer = setTimeout(refreshPreview, 200);
    }

    async function loadBuckets() {
        loadingBuckets = true;
        thresholdKb = Math.round(initialHashMinSize / 1024);
        try {
            buckets = await getSizeBuckets(sourceId);
            await refreshPreview();
        } finally {
            loadingBuckets = false;
        }
    }

    $effect(() => {
        if (open) {
            void loadBuckets();
        }
    });

    function confirm() {
        clearTimeout(previewTimer);
        onConfirm(thresholdKb * 1024);
    }
</script>

<Dialog.Root bind:open>
    <Dialog.Content
        class="sm:max-w-lg"
        showCloseButton={false}
        escapeKeydownBehavior="ignore"
        interactOutsideBehavior="ignore">
        <Dialog.Header>
            <Dialog.Title>Review "{label}" before hashing</Dialog.Title>
            <Dialog.Description>
                Real size distribution for this scan -- fine-tune the hashing
                threshold before the content-hash pass starts.
            </Dialog.Description>
        </Dialog.Header>

        {#if loadingBuckets}
            <div
                class="flex items-center justify-center gap-2 py-8 text-sm text-muted-foreground">
                <Icon icon="ph:spinner-gap-bold" class="animate-spin" />
                Loading size distribution…
            </div>
        {:else}
            <div class="flex flex-col gap-4 py-2">
                <div
                    class="max-h-64 overflow-y-auto rounded-md ring-1 ring-foreground/10">
                    <table class="w-full border-collapse text-xs">
                        <thead>
                            <tr class="bg-muted/50 text-muted-foreground">
                                <th class="px-2 py-1 text-left font-medium"
                                    >Size</th>
                                <th class="px-2 py-1 text-right font-medium"
                                    >Files</th>
                                <th class="px-2 py-1 text-right font-medium"
                                    >GiB</th>
                                <th class="px-2 py-1 text-right font-medium"
                                    >% bytes</th>
                                <th class="px-2 py-1 text-right font-medium"
                                    >% files</th>
                            </tr>
                        </thead>
                        <tbody>
                            {#each buckets as b (b.bucket)}
                                <tr
                                    class="border-t border-foreground/5 tabular-nums">
                                    <td class="px-2 py-1"
                                        >{b.bucket.replace(
                                            /^\d+\.\s*/,
                                            ''
                                        )}</td>
                                    <td class="px-2 py-1 text-right"
                                        >{b.files.toLocaleString()}</td>
                                    <td class="px-2 py-1 text-right"
                                        >{b.gib.toFixed(2)}</td>
                                    <td class="px-2 py-1 text-right"
                                        >{b.pct_bytes.toFixed(2)}%</td>
                                    <td class="px-2 py-1 text-right"
                                        >{b.pct_files.toFixed(2)}%</td>
                                </tr>
                            {/each}
                        </tbody>
                    </table>
                    {#if buckets.length === 0}
                        <p
                            class="px-2 py-3 text-center text-xs text-muted-foreground">
                            No files found.
                        </p>
                    {/if}
                </div>

                <div class="flex flex-col gap-1.5">
                    <Label for="refine-hash-min-size-kb"
                        >Skip hashing below (KB)</Label>
                    <Input
                        id="refine-hash-min-size-kb"
                        type="number"
                        min="0"
                        bind:value={thresholdKb}
                        oninput={thresholdChanged} />
                </div>

                <p class="text-xs text-muted-foreground">
                    {#if preview}
                        <strong
                            class="font-heading text-foreground tabular-nums"
                            >{preview.files_above_threshold.toLocaleString()}</strong>
                        of {preview.total_files.toLocaleString()} files ({formatBytes(
                            preview.bytes_above_threshold
                        )} of {formatBytes(preview.total_bytes)}) will be
                        hashed.
                    {:else}
                        &nbsp;
                    {/if}
                </p>
                <p class="text-xs text-muted-foreground">
                    {#if preview}
                        The remaining files can still be compared by metadata,
                        thus saving scan time.
                    {:else}
                        &nbsp;
                    {/if}
                </p>
            </div>
        {/if}

        <Dialog.Footer>
            <Button disabled={loadingBuckets} onclick={confirm}
                >Start Hashing</Button>
        </Dialog.Footer>
    </Dialog.Content>
</Dialog.Root>
