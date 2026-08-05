// Tracks long-running operations (folder scans, dedup analysis) so the UI can
// show a floating progress card and grey out the button that would start a
// second task of the same kind. Same class-singleton runes pattern as
// `app.svelte.ts`.

export type TaskKind = 'scan' | 'dedup';
export type TaskStatus = 'running' | 'success' | 'error';

export interface Task {
    id: string;
    kind: TaskKind;
    label: string;
    phase?: string;
    current: number;
    total: number; // 0 = indeterminate
    status: TaskStatus;
    message?: string;
    /** Set once the source a hashing phase applies to is known, so a Cancel
     *  button can target `cancel_hash_scan` at the right source. */
    sourceId?: number;
    /** True while a cancel request is in flight, to disable the Cancel
     *  button and avoid double-submitting it. */
    cancelling?: boolean;
}

class TaskTrayState {
    tasks = $state<Task[]>([]);

    /** True if a task of this kind is still running -- the same-kind lock. */
    hasActive(kind: TaskKind): boolean {
        return this.tasks.some(t => t.kind === kind && t.status === 'running');
    }

    start(kind: TaskKind, label: string): string {
        const id = crypto.randomUUID();
        this.tasks = [
            ...this.tasks,
            { id, kind, label, current: 0, total: 0, status: 'running' },
        ];
        return id;
    }

    update(id: string, patch: Partial<Task>) {
        this.tasks = this.tasks.map(t => (t.id === id ? { ...t, ...patch } : t));
    }

    resolve(id: string, status: 'success' | 'error', message?: string) {
        this.update(id, { status, message });
    }

    dismiss(id: string) {
        this.tasks = this.tasks.filter(t => t.id !== id);
    }
}

export const taskTray = new TaskTrayState();
