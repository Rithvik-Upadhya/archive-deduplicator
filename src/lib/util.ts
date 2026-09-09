// Small formatting helpers shared across components.

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

/** Convert the `tree`-style timestamp (YYYY-MM-DD_HH:MM:SS) to a friendlier form. */
export function formatTime(time: string | null): string {
    if (!time) return '—';
    return time.replace('_', ' ');
}

/** Clamp a percentage into 0-100 and round to one decimal. */
export function pct(value: number): number {
    return Math.round(Math.max(0, Math.min(100, value)) * 10) / 10;
}

/* --- Colour scales ---------------------------------------------------------
 *
 * "% duplicated" and match confidence are magnitudes, so they get scales
 * rather than a flat brand tint. A 0.1%-duplicated folder and a 56% one used
 * to render identically; now only the ones worth acting on carry colour.
 */

export type DupLevel = 'none' | 'low' | 'mid' | 'high';

/** Bucket a "portion duplicated" percentage by how much it's worth reclaiming. */
export function dupLevel(percent: number): DupLevel {
    if (percent >= 50) return 'high';
    if (percent >= 20) return 'mid';
    if (percent >= 5) return 'low';
    return 'none';
}

/** Outline-badge classes for a "% dup" figure. */
export const DUP_BADGE: Record<DupLevel, string> = {
    none: 'border-border text-muted-foreground',
    low: 'border-border text-foreground',
    mid: 'border-warn/45 text-warn',
    high: 'border-brand/50 text-brand',
};

/** Fill for the `Progress` indicator behind a "% dup" figure. */
export const DUP_BAR: Record<DupLevel, string> = {
    none: '[&_[data-slot=progress-indicator]]:bg-muted-foreground/40',
    low: '[&_[data-slot=progress-indicator]]:bg-muted-foreground/70',
    mid: '[&_[data-slot=progress-indicator]]:bg-warn',
    high: '[&_[data-slot=progress-indicator]]:bg-brand',
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
 * root, so the result is the folder path the user asked for.
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
 * Copy a folder path to the clipboard. The Tauri webview serves the app over a
 * custom protocol, which is a secure context, so `navigator.clipboard` is
 * available without the clipboard-manager plugin.
 *
 * Returns whether the write succeeded, so a caller can surface a failure rather
 * than silently appearing to have copied.
 */
export async function copyFolderPath(
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
