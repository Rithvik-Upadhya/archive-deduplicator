// Small formatting helpers shared across components.

/** Format a byte count into a human-readable string. */
export function formatBytes(bytes: number): string {
    if (bytes === 0) return '0 B';
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
