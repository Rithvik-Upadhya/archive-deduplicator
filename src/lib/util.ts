// Small formatting helpers shared across components.

import type { NodeType, Source } from './types';

/** Format a byte count into a human-readable string. */
export function formatBytes(bytes: number): string {
    if (bytes === 0) return '0 B';
    if (bytes < 0) return `-${formatBytes(-bytes)}`;
    const units = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
    const i = Math.min(
        units.length - 1,
        Math.floor(Math.log(bytes) / Math.log(1024)),
    );
    const value = bytes / Math.pow(1024, i);
    return `${value.toFixed(value >= 10 || i === 0 ? 0 : 1)} ${units[i]}`;
}

/**
 * Length of the widest string `formatBytes` can return for any value in
 * [0, max] -- how wide a column has to be to hold every figure under a total.
 *
 * Width is not monotonic in the value: "1023 GB" (7 chars) is wider than
 * "1 TB" (4), so a column sized from `formatBytes(max)` alone would clip the
 * very rows it exists to fit. The widest string in each unit tier sits just
 * below the next boundary, so check those.
 */
export function widestBytesWidth(max: number): number {
    let widest = formatBytes(max).length;
    for (let i = 1; 1024 ** i - 1 <= max; i++) {
        widest = Math.max(widest, formatBytes(1024 ** i - 1).length);
    }
    return widest;
}

/** Convert the `tree`-style timestamp (YYYY-MM-DD_HH:MM:SS) to a friendlier form. */
export function formatTime(time: string | null): string {
    if (!time) return '—';
    return time.replace('_', ' ');
}

/** Clamp a percentage into 0-100 and round to one decimal. */
export function pct(value: number): number {
    return Math.round(Math.max(0, Math.min(100, value)) * 10) / 10;
}

/**
 * Same, to two decimals -- for tooltips, where a share that reads `0%` on a
 * badge is still worth seeing. Returns a number rather than a fixed-width
 * string, so a half-duplicated device reads `50%` and not `50.00%`.
 */
export function pct2(value: number): number {
    return Math.round(Math.max(0, Math.min(100, value)) * 100) / 100;
}

/* --- Source colours --------------------------------------------------------
 *
 * Each source gets an identity colour, used to attribute nodes in the
 * consolidated tree to the device they came off. Hex, because the header's
 * native `<input type="color">` only speaks `#rrggbb`.
 *
 * The default palette leaves out grey (reserved for "no source") and the
 * brand crimson / warning yellow, which already mean "duplicated elsewhere"
 * and "not judged".
 */

export const SOURCE_PALETTE = [
    '#3b82f6',
    '#14b8a6',
    '#8b5cf6',
    '#f97316',
    '#ec4899',
    '#22c55e',
    '#06b6d4',
    '#6366f1',
] as const;

/** Bar colour for a node with no (surviving) origin source. */
export const NO_SOURCE_COLOR = 'color-mix(in oklab, var(--muted-foreground) 55%, transparent)';

/**
 * A source's identity colour: the user's pick, else a palette slot keyed on
 * the id rather than list position, so removing one source never recolours
 * the others.
 */
export function sourceColor(source: { id: number; color: string | null }): string {
    return source.color ?? SOURCE_PALETTE[source.id % SOURCE_PALETTE.length];
}

/** The source-bar column of one end-state tree row: one colour per bar,
 *  own source first, plus the tooltip naming them. */
export interface SourceBars {
    colors: string[];
    title: string;
}

/** What source attribution needs from a row -- shared by the Consolidate
 *  tree's `ConsolidationNode` and the Fix Paths tree's `PathTreeNode`. */
type AttributedNode = {
    id: number;
    parent_id: number | null;
    type: NodeType;
    origin_source_id: number | null;
};

/**
 * Source bars for every row of an end-state tree, as a lookup. The one
 * implementation behind both the Consolidate and Fix Paths trees, so the two
 * cannot drift apart on what a folder is said to contain.
 *
 * A row shows its own source first (grey when it has none); a folder then
 * adds every other source among its descendants, in source-list order so a
 * given device always sits in the same slot. Descendant *folders* count as
 * well as files, so an otherwise empty folder dragged in from another device
 * still shows on its ancestors.
 */
export function sourceBarsFor(
    nodes: AttributedNode[],
    sources: Source[],
): (node: AttributedNode) => SourceBars {
    const parentOf = new Map(nodes.map((n) => [n.id, n.parent_id]));
    const sourceById = new Map(sources.map((s) => [s.id, s]));

    // Each node adds its source to its whole ancestor chain; once an ancestor
    // already holds it, every ancestor above does too, so the walk stops there.
    const descSources = new Map<number, Set<number>>();
    for (const n of nodes) {
        const sid = n.origin_source_id;
        if (sid == null) continue;
        let pid = n.parent_id;
        while (pid != null) {
            let set = descSources.get(pid);
            if (!set) descSources.set(pid, (set = new Set()));
            else if (set.has(sid)) break;
            set.add(sid);
            pid = parentOf.get(pid) ?? null;
        }
    }

    return (node) => {
        const own =
            node.origin_source_id == null
                ? undefined
                : sourceById.get(node.origin_source_id);
        const colors = [own ? sourceColor(own) : NO_SOURCE_COLOR];
        let title = own ? `From ${own.device_label}` : 'Not from any device';
        if (node.type === 'directory') {
            const rest = [...(descSources.get(node.id) ?? [])]
                .filter((id) => id !== own?.id)
                .map((id) => sourceById.get(id))
                .filter((s) => s != null)
                .sort((a, b) => a.id - b.id);
            colors.push(...rest.map(sourceColor));
            if (rest.length > 0) {
                title += ` · ${own ? 'also contains' : 'contains'} ${rest
                    .map((s) => s.device_label)
                    .join(', ')}`;
            }
        }
        return { colors, title };
    };
}

/* --- Colour scales ---------------------------------------------------------
 *
 * Duplicate markers are coloured by *where the other copies live*, not by how
 * many bytes are involved. "How much" is already the number printed on the
 * badge; what the user actually decides on is whether deleting this thing
 * would lose the only copy on their shelf. So an item that also exists on
 * another device reads brand-red, and one duplicated only inside its own
 * device reads a quiet grey.
 *
 * `--warn` keeps its own meaning (skipped / not judged) and is deliberately
 * not used for duplication, so a yellow mark always means "no verdict" rather
 * than "some duplication".
 */

export type DupTone = 'external' | 'internal';

/** Outline-badge classes for a duplicate marker, by where the other copies live. */
export const DUP_TONE: Record<DupTone, string> = {
    external: 'border-brand/45 text-brand',
    internal: 'border-muted-foreground/45 text-muted-foreground',
};

/**
 * Text colour for a match-confidence percentage. Certain matches read calm and
 * green — they're the safe ones to act on — while weak matches pick up the
 * warning tint, so the eye lands on what actually needs judgement.
 */
export function confidenceTone(percent: number): string {
    if (percent >= 95) return 'text-ok';
    if (percent >= 75) return 'text-foreground';
    return 'text-warn';
}

/* --- Task tray -------------------------------------------------------- */

import type { TaskStatus } from './stores/tasks.svelte';
import type { IconName } from './components/Icon.svelte';

/** Icon + text colour for a task tray card's status glyph. */
export const TASK_STATUS_ICON: Record<TaskStatus, { icon: IconName; class: string }> = {
    running: { icon: 'ph:spinner-gap-fill', class: 'animate-spin text-muted-foreground' },
    success: { icon: 'ph:check-circle-fill', class: 'text-ok' },
    error: { icon: 'ph:x-circle-fill', class: 'text-destructive' },
};

/** Fill for the `Progress` indicator on a determinate task tray card. */
export const TASK_STATUS_BAR: Record<TaskStatus, string> = {
    running: '[&_[data-slot=progress-indicator]]:bg-muted-foreground/70',
    success: '[&_[data-slot=progress-indicator]]:bg-ok',
    error: '[&_[data-slot=progress-indicator]]:bg-destructive',
};

/* --- Folder paths ------------------------------------------------------- */

/**
 * Join path segments with the host OS separator.
 *
 * Accepts either an already-built path (whose own separators are normalized --
 * `nodes.rel_path` is always stored with '/') or a list of segments walked from
 * a tree's root. Neither form carries a device or source name: `rel_path` is
 * relative to the scan root, and a consolidation walk stops at a user-made
 * root, so the result is the path the user asked for and nothing more.
 */
export function joinPath(path: string | string[], sep: string): string {
    const segments = Array.isArray(path) ? path : path.split(/[/\\]/);
    return segments.filter(s => s.length > 0).join(sep);
}

/**
 * Walk a node's parent chain and return its path segments, root-first.
 *
 * Shared by the consolidation and Fix Paths trees: same algorithm, different id
 * spaces, so it lives here rather than being written out twice. Both trees walk
 * to a consolidation root -- a folder the user made or dropped -- so the result
 * never contains a device or source name.
 */
export function pathSegments<
    T extends { id: number; name: string; parent_id: number | null },
>(byId: Map<number, T>, id: number): string[] {
    const segments: string[] = [];
    let cur: number | null = id;
    // A malformed parent chain would otherwise spin forever inside a render.
    const seen = new Set<number>();
    while (cur != null && !seen.has(cur)) {
        seen.add(cur);
        const node = byId.get(cur);
        if (!node) break;
        segments.unshift(node.name);
        cur = node.parent_id;
    }
    return segments;
}

/**
 * Copy a path -- a file's or a folder's -- to the clipboard. The Tauri webview
 * serves the app over a custom protocol, which is a secure context, so
 * `navigator.clipboard` is available without the clipboard-manager plugin.
 *
 * Returns whether the write succeeded, for callers that want to report a
 * failure. Note that no caller currently checks it, so a clipboard failure is
 * silent today: the button looks like it worked. Wiring this into `taskTray`
 * would fix that.
 */
export async function copyPath(
    path: string | string[],
    sep: string,
): Promise<boolean> {
    try {
        await navigator.clipboard.writeText(joinPath(path, sep));
        return true;
    } catch {
        return false;
    }
}

/**
 * For every node matching `match`, adds 1 to the count of each of its
 * ancestors -- so a row can say "N such items anywhere beneath me", even
 * while collapsed. The one helper both consolidated trees use for their
 * after-name indicators (renamed contents, struck contents). A node never
 * counts toward itself.
 */
export function countBelow<T extends { id: number; parent_id: number | null }>(
    nodes: T[],
    match: (node: T) => boolean
): Map<number, number> {
    const parentOf = new Map(nodes.map(n => [n.id, n.parent_id]));
    const counts = new Map<number, number>();
    for (const n of nodes) {
        if (!match(n)) continue;
        let pid = n.parent_id;
        // A malformed parent chain would otherwise spin forever inside a render.
        const seen = new Set<number>();
        while (pid != null && !seen.has(pid)) {
            seen.add(pid);
            counts.set(pid, (counts.get(pid) ?? 0) + 1);
            pid = parentOf.get(pid) ?? null;
        }
    }
    return counts;
}

/**
 * The strikethrough toggle shared by the Consolidate and Fix Paths toolbars.
 * `roots` are the selection's roots and `effective` is every row they select
 * (roots plus descendants). If every root is already struck (by its own flag
 * or an ancestor's), clear the flag on every selected row, so a descendant
 * struck on its own doesn't survive the un-strike. Otherwise strike the roots
 * alone; their descendants inherit it.
 */
export function strikeToggle(
    roots: number[],
    effective: number[],
    isStruck: (id: number) => boolean
): { ids: number[]; value: boolean } {
    return roots.every(isStruck)
        ? { ids: effective, value: false }
        : { ids: roots, value: true };
}
