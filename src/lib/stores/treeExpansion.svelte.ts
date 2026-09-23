import { nodeLocation } from '$lib/api';
import { taskTray } from '$lib/stores/tasks.svelte';

// Module-level singletons so expand/collapse state (and the per-device
// filter toggle) survive component unmount -- switching views, or
// collapsing/re-expanding a device panel, both destroy and recreate the
// TreeItem/ConsolidationNodeItem/PathTreeItem instances that would otherwise
// hold this in local `$state`.

export class ToggleSet {
    #ids = $state<Set<number>>(new Set());
    /** Plain (non-reactive) on purpose -- bookkeeping for `seedOnce` only,
     *  never read by a template. */
    #seeded = new Set<number>();

    has(id: number): boolean {
        return this.#ids.has(id);
    }

    add(id: number) {
        if (this.#ids.has(id)) return;
        const next = new Set(this.#ids);
        next.add(id);
        this.#ids = next;
    }

    delete(id: number) {
        if (!this.#ids.has(id)) return;
        const next = new Set(this.#ids);
        next.delete(id);
        this.#ids = next;
    }

    toggle(id: number) {
        const next = new Set(this.#ids);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        this.#ids = next;
    }

    /** Applies `value` once, on this id's first-ever encounter, without
     *  clobbering a later explicit toggle -- e.g. auto-expanding an
     *  over-limit path branch on first sight while still letting the user
     *  collapse it and have that choice stick. */
    seedOnce(id: number, value: boolean) {
        if (this.#seeded.has(id)) return;
        this.#seeded.add(id);
        if (value) this.add(id);
    }
}

/** Expanded node ids for source-device trees (`nodes.id`) -- shared between
 *  the Deduplicate view and the Consolidate view's source panel, since both
 *  render the same `DeviceTree`/`TreeItem` for the same devices. */
export const deviceTreeExpanded = new ToggleSet();

/** Collapsed device-panel ids (`sources.id`). Presence means collapsed;
 *  absence (the common case) means expanded, matching `DeviceTree`'s
 *  default-open behavior. */
export const deviceCollapsed = new ToggleSet();

/** Per-device "exclusive to this device" filter toggle (`sources.id`). */
export const deviceFilterOn = new ToggleSet();

/** Expanded node ids for the consolidation end-state tree
 *  (`consolidation_nodes.id`) -- shared between the Consolidate view and the
 *  Fix Paths view, since both render the same underlying tree. */
export const consolidationTreeExpanded = new ToggleSet();

/** A device-tree node waiting to be scrolled to and selected. Set by
 *  `revealInDeviceTree`; the node's `TreeItem` picks it up whenever its row
 *  mounts -- which, in a lazily loaded tree, may be several fetches later --
 *  and clears it. */
export const deviceReveal = new (class {
    target = $state<number | null>(null);
})();

/**
 * Show a source node in its device tree without leaving the current view:
 * lift the device's funnel if (and only if) it hides the node, open the
 * device panel, expand every ancestor, then hand the node to `deviceReveal`
 * so its row scrolls into view and selects itself once it mounts.
 */
export async function revealInDeviceTree(m: { node_id: number; source_id: number }) {
    try {
        const loc = await nodeLocation(m.node_id);
        if (loc.hidden_by_filter) deviceFilterOn.delete(m.source_id);
        deviceCollapsed.delete(m.source_id);
        for (const id of loc.ancestors) deviceTreeExpanded.add(id);
        deviceReveal.target = m.node_id;
    } catch (err) {
        taskTray.notify('Could not show in device tree', 'error', String(err));
    }
}
