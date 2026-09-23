import { ToggleSet } from './treeExpansion.svelte';
import type { NodeSearchResult } from '../types';
import { compileNameQuery } from '../util';

/** Above this many matches a search does not open every folder leading to
 *  one: the device trees load a level per fetch, and expanding thousands of
 *  folders at once freezes the webview. The filter itself is never capped. */
export const AUTO_EXPAND_LIMIT = 500;

/**
 * The top-bar name search. Pure state -- the store in `app.svelte.ts` drives
 * it (running the backend search, refreshing groups), so this module imports
 * nothing from there.
 *
 * `applied` is what the views filter and highlight by; `query` and
 * `caseSensitive` are only the input's pending values until the user presses
 * Enter or the search button.
 */
class SearchState {
    /** The input's text. */
    query = $state('');
    /** The case toggle's pending value. */
    caseSensitive = $state(false);
    /** A search is in flight. */
    busy = $state(false);
    /** The search in effect; null when not searching. */
    applied = $state<{ query: string; caseSensitive: boolean } | null>(null);

    /** Device-tree nodes whose name matches. */
    deviceMatched = $state<Set<number>>(new Set());
    /** Matches plus their ancestors: every device-tree row a search keeps. */
    deviceVisible = $state<Set<number>>(new Set());

    /** Expansion used by the trees *while searching*, in place of
     *  `deviceTreeExpanded`/`consolidationTreeExpanded`, so opening and
     *  closing folders during a search leaves the normal expansion as it was
     *  for when the search is cleared. Replaced on every search. */
    deviceExpanded = $state(new ToggleSet());
    consExpanded = $state(new ToggleSet());

    /** The applied search, compiled once: the matcher behind every
     *  highlight, the matched-member label and the consolidated tree's
     *  filter, so none of them can disagree about wildcards. */
    compiled = $derived(
        this.applied
            ? compileNameQuery(this.applied.query, this.applied.caseSensitive)
            : null
    );

    /** Bumped on every apply and reset, for views that react to either. */
    generation = $state(0);

    get active(): boolean {
        return this.applied !== null;
    }

    /** Record a completed backend search as the one in effect. */
    apply(query: string, caseSensitive: boolean, res: NodeSearchResult) {
        this.deviceMatched = new Set(res.matched);
        this.deviceVisible = new Set([...res.matched, ...res.ancestors]);
        const expanded = new ToggleSet();
        if (res.matched.length <= AUTO_EXPAND_LIMIT) {
            for (const id of res.ancestors) expanded.add(id);
        }
        this.deviceExpanded = expanded;
        this.consExpanded = new ToggleSet();
        this.applied = { query, caseSensitive };
        this.generation++;
    }

    /** Back to not searching, input included. */
    reset() {
        this.query = '';
        this.caseSensitive = false;
        this.applied = null;
        this.deviceMatched = new Set();
        this.deviceVisible = new Set();
        this.deviceExpanded = new ToggleSet();
        this.consExpanded = new ToggleSet();
        this.generation++;
    }

    /** Whether `name` matches the applied search (false when not searching). */
    matches(name: string): boolean {
        return this.compiled?.test(name) ?? false;
    }
}

export const search = new SearchState();
