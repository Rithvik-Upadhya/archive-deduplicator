// Shift/cmd-click multi-selection for a recursive tree of draggable rows.
// One instance is shared (via prop-drilling, matching this codebase's
// existing recursive-component convention) across every row in one logical
// tree -- e.g. all source devices in the Consolidate view, or the
// consolidation tree itself. A row registers its DOM element on mount via
// the `selectable` action; shift-click range selection is computed from
// actual DOM order at click time (via compareDocumentPosition) rather than
// a maintained order array, which sidesteps ordering problems from lazily-
// loaded async children -- only currently-mounted (i.e. visible) rows are
// registered, so a range naturally skips collapsed subtrees.
//
// Selection is hierarchical. `selected` stores only *roots*, and a row
// counts as selected when it or any ancestor is a root -- so selecting a
// folder selects everything beneath it, including children not yet loaded.
// A child of a selected folder cannot be toggled on its own (ctrl/cmd-click
// on it is a no-op), and no root ever sits beneath another. Every operation
// therefore receives each item by exactly one route: moving or copying a
// folder and, separately, one of its own children is not expressible.

import { SvelteMap } from 'svelte/reactivity';

export class TreeSelection<Meta = unknown> {
    /** Selection roots. Read through `isSelected` / `roots()`; a descendant
     *  of a root is selected without appearing here. */
    selected = $state<Set<number>>(new Set());
    private anchor: number | null = null;
    private elements = new Map<number, HTMLElement>();
    private meta = new Map<number, Meta>();
    /** id -> parent id for every mounted row. Reactive because `isSelected`
     *  walks it: a freshly expanded child of a selected folder registers
     *  after its first render, and must then re-render as selected. A
     *  mounted row's ancestors are always mounted, so the chain is complete
     *  for every row that can be asked about. */
    private parents = new SvelteMap<number, number | null>();

    register(id: number, el: HTMLElement, meta?: Meta, parentId?: number | null) {
        this.elements.set(id, el);
        if (meta !== undefined) this.meta.set(id, meta);
        this.parents.set(id, parentId ?? null);
    }

    /** `el`, when given, must be the element `id` is registered with: a row
     *  moved under a new parent remounts, and the new instance can register
     *  before the old one is destroyed -- whose late unregister must not
     *  wipe the fresh registration (or deselect the row). */
    unregister(id: number, el?: HTMLElement) {
        if (el && this.elements.get(id) !== el) return;
        this.elements.delete(id);
        this.meta.delete(id);
        this.parents.delete(id);
        if (this.anchor === id) this.anchor = null;
        // A root that's gone (deleted, moved elsewhere, or its parent
        // collapsed) can no longer sensibly be "selected" -- keeps
        // `selected` from silently drifting out of sync with what's
        // actually on screen after any tree mutation. Descendants of a
        // selected folder are not roots, so collapsing it loses nothing.
        if (this.selected.has(id)) {
            const next = new Set(this.selected);
            next.delete(id);
            this.selected = next;
        }
    }

    getMeta(id: number): Meta | undefined {
        return this.meta.get(id);
    }

    /** Whether `id` or any of its ancestors is a selection root. */
    isSelected(id: number): boolean {
        const sel = this.selected;
        let cur: number | null | undefined = id;
        while (cur != null) {
            if (sel.has(cur)) return true;
            cur = this.parents.get(cur);
        }
        return false;
    }

    /** Whether some *proper* ancestor of `id` is in `set`. */
    private hasAncestorIn(id: number, set: Set<number>): boolean {
        let cur = this.parents.get(id);
        while (cur != null) {
            if (set.has(cur)) return true;
            cur = this.parents.get(cur);
        }
        return false;
    }

    /** `ids` minus every id lying beneath another id in `ids`. */
    private normalize(ids: Iterable<number>): Set<number> {
        const set = new Set(ids);
        return new Set([...set].filter(id => !this.hasAncestorIn(id, set)));
    }

    /** The selection as a clean list: each selected item reached by exactly
     *  one route. Re-normalised on read as a safety net for a move that has
     *  just carried one root beneath another. Every operation on the
     *  selection should consume this. */
    roots(): number[] {
        return [...this.normalize(this.selected)];
    }

    /** Apply shift/cmd|ctrl/plain click semantics for a click on `id`.
     *  Accepts a KeyboardEvent too so Enter-key activation gets identical
     *  modifier-key handling to a real click. */
    click(id: number, e: { shiftKey: boolean; metaKey: boolean; ctrlKey: boolean }) {
        if (e.shiftKey && this.anchor != null && this.elements.has(this.anchor)) {
            this.selected = this.normalize(this.rangeBetween(this.anchor, id));
            return;
        }
        if (e.metaKey || e.ctrlKey) {
            // Already selected through a folder above it: a child of a
            // selected folder is not individually toggleable.
            if (this.hasAncestorIn(id, this.selected)) return;
            const next = new Set(this.selected);
            if (next.has(id)) {
                next.delete(id);
            } else {
                next.add(id);
                this.anchor = id;
            }
            // Adding a folder absorbs any roots already beneath it.
            this.selected = this.normalize(next);
            return;
        }
        // Plain click, or a shift-click with no usable anchor (never set,
        // or the anchor row is no longer mounted).
        this.selected = new Set([id]);
        this.anchor = id;
    }

    /** Ids to drag starting from `id`: the whole selection (as roots) if `id`
     *  is part of it, otherwise just `id` alone.
     *  Deliberately does not mutate selection state -- doing so mid-
     *  dragstart risks the browser treating the row as having changed out
     *  from under the drag and cancelling it. */
    dragIds(id: number): number[] {
        return this.isSelected(id) ? this.roots() : [id];
    }

    clear() {
        this.selected = new Set();
        this.anchor = null;
    }

    private rangeBetween(a: number, b: number): number[] {
        const entries = [...this.elements.entries()];
        entries.sort((x, y) => {
            const pos = x[1].compareDocumentPosition(y[1]);
            if (pos & Node.DOCUMENT_POSITION_FOLLOWING) return -1;
            if (pos & Node.DOCUMENT_POSITION_PRECEDING) return 1;
            return 0;
        });
        const ids = entries.map(([id]) => id);
        const ia = ids.indexOf(a);
        const ib = ids.indexOf(b);
        if (ia === -1 || ib === -1) return [b];
        const [lo, hi] = ia < ib ? [ia, ib] : [ib, ia];
        return ids.slice(lo, hi + 1);
    }
}

type SelectableParams<Meta> = {
    selection?: TreeSelection<Meta>;
    id: number;
    /** The row's parent id in the same tree (`null` at a root), so that
     *  selecting a folder selects its descendants. */
    parentId: number | null;
    meta?: Meta;
};

/** Svelte action registering `id`'s element with `selection` for the
 *  lifetime of the element. A no-op when `selection` is omitted, so it can
 *  stay in the template unconditionally for components (like TreeItem)
 *  that are only sometimes used in a selectable context. */
export function selectable<Meta>(el: HTMLElement, params: SelectableParams<Meta>) {
    params.selection?.register(params.id, el, params.meta, params.parentId);
    return {
        // Svelte passes a fresh params object on every re-render; without
        // this, `destroy()` would close over the *first* render's params
        // and re-register under a stale id if `id`/`selection` ever change
        // for a mounted instance (unlikely with keyed {#each}, but cheap to
        // handle correctly rather than assume).
        update(next: SelectableParams<Meta>) {
            if (next.id !== params.id || next.selection !== params.selection) {
                params.selection?.unregister(params.id, el);
                params = next;
                params.selection?.register(params.id, el, params.meta, params.parentId);
            } else if (next.parentId !== params.parentId) {
                // Moved under a new parent without remounting: re-register
                // in place (register overwrites; unregister would also drop
                // this row from the selection).
                params = next;
                params.selection?.register(params.id, el, params.meta, params.parentId);
            } else {
                params = next;
            }
        },
        destroy() {
            params.selection?.unregister(params.id, el);
        },
    };
}
