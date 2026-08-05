<script lang="ts">
    import { app } from '$lib/stores/app.svelte';
    import { detectMedium, hashSettingsGet } from '$lib/api';
    import type { MediumKind, ScanConfig } from '$lib/types';
    import { Button } from '$lib/components/ui/button';
    import { Input } from '$lib/components/ui/input';
    import { Label } from '$lib/components/ui/label';
    import * as Dialog from '$lib/components/ui/dialog';
    import * as ToggleGroup from '$lib/components/ui/toggle-group';
    import Icon from '@iconify/svelte';
    import { taskTray } from '$lib/stores/tasks.svelte';

    let {
        open = $bindable(false),
        path,
        label,
        onConfirm,
    }: {
        open: boolean;
        path: string;
        label: string;
        onConfirm: (config: ScanConfig) => void;
    } = $props();

    const MEDIUM_KINDS: { id: MediumKind; label: string }[] = [
        { id: 'hdd', label: 'HDD' },
        { id: 'ssd', label: 'SSD' },
        { id: 'network', label: 'Network' },
        { id: 'optical', label: 'Optical' },
        { id: 'unknown', label: 'Unknown' },
    ];

    let step = $state<1 | 2>(1);
    let loading = $state(true);

    // Step 1 -- hashing spec (bytes, denominated in KiB in the UI, matching
    // the dedup tuning sliders' KB convention elsewhere in this app).
    let samplingMode = $state<'full' | 'sampled'>('sampled');
    let thresholdKb = $state(8192); // 8 MiB
    let probeKb = $state(1024); // 1 MiB
    let strideKb = $state(32768); // 32 MiB
    let specLocked = $state(false);

    // Step 2 -- per-source scan options.
    let hashingEnabled = $state(true);
    let hashMinSizeKb = $state(64); // 64 KiB, matches the matcher's own default floor
    let mediumOverride = $state<MediumKind>('unknown');
    let filesystemOverride = $state('');

    async function loadDefaults() {
        loading = true;
        try {
            const [medium, settings] = await Promise.all([
                detectMedium(path),
                app.activeWorkspaceId != null
                    ? hashSettingsGet(app.activeWorkspaceId)
                    : Promise.resolve(null),
            ]);
            mediumOverride = medium.medium_kind;
            filesystemOverride = medium.filesystem ?? '';

            if (settings) {
                specLocked = settings.locked;
                samplingMode = settings.spec.threshold == null ? 'full' : 'sampled';
                if (settings.spec.threshold != null) {
                    thresholdKb = Math.round(settings.spec.threshold / 1024);
                }
                probeKb = Math.round(settings.spec.probe / 1024);
                strideKb = Math.round(settings.spec.stride / 1024);
            }
        } catch (err) {
            taskTray.notify('Detection failed', 'error', String(err));
        } finally {
            loading = false;
        }
    }

    // Re-detect and reload workspace hash settings every time the dialog is
    // opened for a new folder -- `path` changes on every "Scan Folder" click.
    $effect(() => {
        if (open) {
            step = 1;
            hashingEnabled = true;
            void loadDefaults();
        }
    });

    function specString(threshold: number | null, probe: number, stride: number): string {
        return threshold == null
            ? 'blake3/v1/full'
            : `blake3/v1/th${threshold}-s${probe}-t${stride}`;
    }

    function confirm() {
        const threshold = samplingMode === 'full' ? null : thresholdKb * 1024;
        const probe = probeKb * 1024;
        const stride = strideKb * 1024;
        onConfirm({
            hashSpec: {
                threshold,
                probe,
                stride,
                spec_string: specString(threshold, probe, stride),
            },
            specLocked,
            hashMinSize: hashMinSizeKb * 1024,
            mediumOverride,
            filesystemOverride: filesystemOverride.trim(),
            hashingEnabled,
        });
        open = false;
    }
</script>

<Dialog.Root bind:open>
    <Dialog.Content class="sm:max-w-md">
        <Dialog.Header>
            <Dialog.Title>Configure scan of "{label}"</Dialog.Title>
            <Dialog.Description>
                {#if step === 1}
                    Step 1 of 2 -- content-hashing spec for this workspace.
                {:else}
                    Step 2 of 2 -- per-device scan options.
                {/if}
            </Dialog.Description>
        </Dialog.Header>

        {#if loading}
            <div class="flex items-center justify-center gap-2 py-8 text-sm text-muted-foreground">
                <Icon icon="ph:spinner-gap-bold" class="animate-spin" />
                Detecting medium…
            </div>
        {:else if step === 1}
            <div class="flex flex-col gap-4 py-2">
                {#if specLocked}
                    <p class="text-xs text-muted-foreground">
                        This workspace already has hashed files. The spec below is
                        locked so every hash stays comparable.
                    </p>
                {/if}
                <div class="flex flex-col gap-1.5">
                    <Label>Hashing mode</Label>
                    <ToggleGroup.Root
                        type="single"
                        variant="outline"
                        size="sm"
                        value={samplingMode}
                        disabled={specLocked}
                        onValueChange={(v) => v && (samplingMode = v as 'full' | 'sampled')}>
                        <ToggleGroup.Item value="sampled" class="px-2 text-xs">
                            Sampled
                        </ToggleGroup.Item>
                        <ToggleGroup.Item value="full" class="px-2 text-xs">
                            Full file
                        </ToggleGroup.Item>
                    </ToggleGroup.Root>
                </div>

                {#if samplingMode === 'sampled'}
                    <div class="flex flex-col gap-1.5">
                        <Label for="threshold-kb">Sample above (KB)</Label>
                        <Input
                            id="threshold-kb"
                            type="number"
                            min="0"
                            disabled={specLocked}
                            bind:value={thresholdKb} />
                        <p class="text-xs text-muted-foreground">
                            Files at or below this size are always fully hashed.
                        </p>
                    </div>
                    <div class="flex flex-col gap-1.5">
                        <Label for="probe-kb">Probe size (KB)</Label>
                        <Input
                            id="probe-kb"
                            type="number"
                            min="1"
                            disabled={specLocked}
                            bind:value={probeKb} />
                    </div>
                    <div class="flex flex-col gap-1.5">
                        <Label for="stride-kb">Stride between probes (KB)</Label>
                        <Input
                            id="stride-kb"
                            type="number"
                            min="1"
                            disabled={specLocked}
                            bind:value={strideKb} />
                    </div>
                {/if}
            </div>
        {:else}
            <div class="flex flex-col gap-4 py-2">
                <div class="flex flex-col gap-1.5">
                    <Label>Content hashing</Label>
                    <ToggleGroup.Root
                        type="single"
                        variant="outline"
                        size="sm"
                        value={hashingEnabled ? 'on' : 'off'}
                        onValueChange={(v) => v && (hashingEnabled = v === 'on')}>
                        <ToggleGroup.Item value="on" class="px-2 text-xs">Enabled</ToggleGroup.Item>
                        <ToggleGroup.Item value="off" class="px-2 text-xs">Disabled</ToggleGroup.Item>
                    </ToggleGroup.Root>
                    <p class="text-xs text-muted-foreground">
                        When disabled, this device is matched on filesystem metadata only --
                        no content hash pass runs for it.
                    </p>
                </div>

                {#if hashingEnabled}
                    <div class="flex flex-col gap-1.5">
                        <Label for="hash-min-size-kb">Skip hashing below (KB)</Label>
                        <Input
                            id="hash-min-size-kb"
                            type="number"
                            min="0"
                            bind:value={hashMinSizeKb} />
                    </div>
                {/if}
                <div class="flex flex-col gap-1.5">
                    <Label>Storage medium</Label>
                    <ToggleGroup.Root
                        type="single"
                        variant="outline"
                        size="sm"
                        value={mediumOverride}
                        onValueChange={(v) => v && (mediumOverride = v as MediumKind)}>
                        {#each MEDIUM_KINDS as m (m.id)}
                            <ToggleGroup.Item value={m.id} class="px-2 text-xs">
                                {m.label}
                            </ToggleGroup.Item>
                        {/each}
                    </ToggleGroup.Root>
                </div>
                <div class="flex flex-col gap-1.5">
                    <Label for="filesystem-override">Filesystem</Label>
                    <Input
                        id="filesystem-override"
                        type="text"
                        placeholder="e.g. NTFS, ext4, APFS"
                        bind:value={filesystemOverride} />
                </div>
            </div>
        {/if}

        <Dialog.Footer>
            {#if step === 2}
                <Button variant="outline" onclick={() => (step = 1)}>Back</Button>
            {/if}
            <Button variant="outline" onclick={() => (open = false)}>Cancel</Button>
            {#if step === 1}
                <Button disabled={loading} onclick={() => (step = 2)}>Next</Button>
            {:else}
                <Button disabled={loading} onclick={confirm}>Scan</Button>
            {/if}
        </Dialog.Footer>
    </Dialog.Content>
</Dialog.Root>
