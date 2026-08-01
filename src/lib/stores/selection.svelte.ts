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

export class TreeSelection<Meta = unknown> {
    selected = $state<Set<number>>(new Set());
    private anchor: number | null = null;
    private elements = new Map<number, HTMLElement>();
    private meta = new Map<number, Meta>();

    register(id: number, el: HTMLElement, meta?: Meta) {
        this.elements.set(id, el);
        if (meta !== undefined) this.meta.set(id, meta);
    }

    unregister(id: number) {
        this.elements.delete(id);
        this.meta.delete(id);
        if (this.anchor === id) this.anchor = null;
        // A row that's gone (deleted, moved elsewhere, or its parent
        // collapsed) can no longer sensibly be "selected" -- keeps
        // `selected` from silently drifting out of sync with what's
        // actually on screen after any tree mutation.
        if (this.selected.has(id)) {
            const next = new Set(this.selected);
            next.delete(id);
            this.selected = next;
        }
    }

    getMeta(id: number): Meta | undefined {
        return this.meta.get(id);
    }

    isSelected(id: number): boolean {
        return this.selected.has(id);
    }

    /** Apply shift/cmd|ctrl/plain click semantics for a click on `id`.
     *  Accepts a KeyboardEvent too so Enter-key activation gets identical
     *  modifier-key handling to a real click. */
    click(id: number, e: { shiftKey: boolean; metaKey: boolean; ctrlKey: boolean }) {
        if (e.shiftKey && this.anchor != null && this.elements.has(this.anchor)) {
            this.selected = new Set(this.rangeBetween(this.anchor, id));
            return;
        }
        if (e.metaKey || e.ctrlKey) {
            const next = new Set(this.selected);
            if (next.has(id)) {
                next.delete(id);
            } else {
                next.add(id);
                this.anchor = id;
            }
            this.selected = next;
            return;
        }
        // Plain click, or a shift-click with no usable anchor (never set,
        // or the anchor row is no longer mounted).
        this.selected = new Set([id]);
        this.anchor = id;
    }

    /** Ids to drag starting from `id`: the current selection if `id` is
     *  already part of a multi-selection, otherwise just `id` alone.
     *  Deliberately does not mutate selection state -- doing so mid-
     *  dragstart risks the browser treating the row as having changed out
     *  from under the drag and cancelling it. */
    dragIds(id: number): number[] {
        if (this.selected.has(id) && this.selected.size > 1) {
            return [...this.selected];
        }
        return [id];
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

/** Svelte action registering `id`'s element with `selection` for the
 *  lifetime of the element. A no-op when `selection` is omitted, so it can
 *  stay in the template unconditionally for components (like TreeItem)
 *  that are only sometimes used in a selectable context. */
export function selectable<Meta>(
    el: HTMLElement,
    params: { selection?: TreeSelection<Meta>; id: number; meta?: Meta }
) {
    params.selection?.register(params.id, el, params.meta);
    return {
        // Svelte passes a fresh params object on every re-render; without
        // this, `destroy()` would close over the *first* render's params
        // and re-register under a stale id if `id`/`selection` ever change
        // for a mounted instance (unlikely with keyed {#each}, but cheap to
        // handle correctly rather than assume).
        update(next: { selection?: TreeSelection<Meta>; id: number; meta?: Meta }) {
            if (next.id !== params.id || next.selection !== params.selection) {
                params.selection?.unregister(params.id);
                params = next;
                params.selection?.register(params.id, el, params.meta);
            } else {
                params = next;
            }
        },
        destroy() {
            params.selection?.unregister(params.id);
        },
    };
}
