# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this app does

Archive Deduplicator is a Tauri 2 desktop app for reconciling several offline archives (old discs,
external drives, backup folders) against each other. **It never reads file contents** — it works
purely from filesystem metadata, either imported from a `tree` JSON dump or from a live folder walk.
Everything lives in one local SQLite database, so all state survives a restart.

The user workflow is three views, in order, and much of the backend design follows from it:

1. **Deduplicate** — import sources, run the matcher, browse match groups and per-device stats.
2. **Consolidate** — drag nodes from source trees into a target ("end-state") tree.
3. **Fix Paths** — walk the *consolidated end-state* tree for paths over the Windows 260-char
   limit and rename components until they fit.

## Commands

Package manager is **pnpm** (pinned in `package.json`). Run everything from the repo root.

```bash
pnpm install
pnpm tauri dev             # full app (starts vite on :1420, then cargo)
pnpm dev                   # frontend only — the UI will fail on every invoke()
pnpm check                 # svelte-kit sync + svelte-check (the TS typecheck)
pnpm tauri build           # production bundle

cd src-tauri
cargo test                 # Rust tests (db.rs, dedup.rs, links.rs, rollup.rs, dbio.rs, consolidate.rs, pathfix.rs)
cargo test detects_cross_source_file_duplicates    # single test
cargo clippy && cargo fmt
```

There is no JS test runner and no ESLint config; `pnpm check` is the only frontend gate.
Formatting is Prettier via `.prettierrc` (4 spaces, single quotes, `bracketSameLine: true`).

### Do not run pnpm from a Linux sandbox

**Claude: never run `pnpm` (or anything that shells out to it) when working from a Linux
container/sandbox. Ask the user to run it and paste back the output.** The `cargo` commands above
are fine to run anywhere.

Two independent reasons:

- `node_modules/` is mounted live from the macOS host and holds darwin-arm64 native binaries
  (`@esbuild/darwin-arm64`, `@rollup/rollup-darwin-arm64`, `@tailwindcss/oxide-darwin-arm64`) plus
  a `storeDir` under `/Users/…/Library/pnpm/store`. On Linux, pnpm treats that as a foreign store
  and wants to **purge and refetch the whole directory** — which would delete the host's working
  install. It bails with `ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY` instead. Do not defeat that
  guard with `CI=true` or `confirmModulesPurge=false`.
- pnpm 11 runs a deps-status check before *every* script, so `pnpm check`, `pnpm dev`, and
  `pnpm tauri dev` all trigger the same implicit `pnpm install` and hit the same wall. There is no
  read-only pnpm subcommand that dodges it.

Running the app from a sandbox is not a workaround either: Tauri's Linux stack (webkit2gtk-4.1,
GTK3, libsoup-3.0) is absent, and there is no display, X server, or Xvfb. `pnpm tauri dev` is a
**macOS-host-only** command.

## Architecture

### The Rust/TS contract

Three files must be kept in lockstep — changing a command signature means editing all three:

- `src-tauri/src/lib.rs` — the `invoke_handler!` list. A command that isn't registered here is
  invisible to the frontend even if it compiles.
- `src-tauri/src/model.rs` ↔ `src/lib/types.ts` — serde structs and their TypeScript mirrors.
  Field names are snake_case on both sides (no serde rename except `type`/`node_type`).
- `src/lib/api.ts` — the only file allowed to call `invoke`. Components import from here.
  Note Tauri's arg convention: Rust `source_id` is passed as `sourceId` from JS.

### Backend module flow

```
parse.rs / scan.rs  →  nodes table  →  links.rs  →  dedup.rs  →  rollup.rs  →  dup_annot cache
   (import)                          (hardlinks)   (matching)   (folders +      (fast tree browsing)
                                                                 device stats)
```

- **`parse.rs`** flattens a `tree -JDs --inodes --device` JSON doc into `FlatNode`s, precomputing
  `subtree_size` / `subtree_file_count` bottom-up so the UI never aggregates. **`scan.rs`** produces
  the same `Flattened` struct from a live `WalkDir`, so both import paths share `insert_nodes`.
  Neither module knows about hardlinks — that's `links.rs`'s job, run as a post-`insert_nodes` step.
- **`links.rs`** detects hardlink alias sets (files sharing `(dev, inode)` within one source) right
  after `insert_nodes`, inside the same transaction. Only trusted where the platform's inode/dev
  values are reliable (currently: Unix live scans only — see `scan_produces_trusted_inodes`; Windows
  scans and `tree` JSON imports are always untrusted pending real per-volume filesystem detection).
  The lowest-`rel_path` member becomes canonical; the rest get `nodes.alias_of` set and a
  `kind='hardlink', confidence=100` match group. Aliases stay fully visible in `get_tree` (the user
  still needs to see every name a physical file has) but are excluded from `subtree_size`/
  `subtree_file_count` propagation into ancestors and from the matcher's candidate pool
  (`dedup.rs::load_files` filters `alias_of IS NULL`) — deleting `k-1` aliases frees zero bytes until
  the last name is gone, so they must never be scored as ordinary duplicates.
- **`dedup.rs`** is the matcher: absolute vetoes (different size via bucketing, different extension,
  zero-byte) followed by three fixed-confidence evidence tiers that never merge or average —
  C (70, exact name + strong mtime), D (55, exact name only), E (45, same parent-folder name +
  some mtime support, names differ). Metadata-tier groups are never built by transitive union-find;
  a group must be a genuine clique (every pair independently qualifies at that tier), enforced via
  exact-name grouping for C/D (name equality is transitive) and Bron–Kerbosch maximal-clique
  enumeration for E (the mtime-tolerance edge condition is not transitive). Two tunables come from
  the UI: `min_size_bytes` (a **hard filter**, default 64 KiB) and `min_confidence` (a tier cutoff
  now, not a continuous threshold). `kind='hardlink'` groups are `links.rs`'s, not this module's —
  the group-clearing `DELETE` at the start of a run explicitly spares them.
- **`rollup.rs`** builds folder-level groups on top of the file groups, then `rebuild_annotations`
  writes the `dup_annot` table (per-node `has_dup` / `dup_pct`) so `get_tree` is a plain query.
  Its byte/leaf rollup queries (`load_file_locs`, `load_leaf_locs`) also filter `alias_of IS NULL`.
- **`pathfix.rs`** walks the *consolidation* tree, not the sources — but descends into `nodes` for
  subtrees dragged in wholesale. Renames of consolidation nodes are written straight to
  `consolidation_nodes.name`; renames of files inside a dragged-in source directory are stored as
  virtual edits in `pathfix_state` and folded in at path-computation and export time.
- **`dbio.rs`** — export is a SQLite backup-API copy of the live file. Import is deliberately
  non-destructive: it attaches the external file read-only and recreates every workspace found as
  a *new* workspace with all foreign keys remapped (including `nodes.alias_of`, which — unlike
  `parent_id` — can reference a node with a *lower* id than itself, since a hardlink's canonical is
  chosen by `rel_path` rather than insertion order; it's backfilled in a second pass once every id
  is mapped). Never make import overwrite.
- **`db.rs`** owns the schema (`init_schema`, `CREATE TABLE IF NOT EXISTS`, safe for brand-new
  databases) plus a real migration step (`migrate`, gated on `PRAGMA user_version` so it stays a
  read-only no-op once a database is current — see the doc comment on why an unconditional write
  there would reintroduce lock contention with `run_dedup`'s dedicated connection). Adding a column
  to an already-shipped table needs an entry in *both* `init_schema`'s DDL (for fresh databases) and
  `migrate`'s `alterations` list (for existing ones) — `CREATE TABLE IF NOT EXISTS` alone only ever
  helps the former. The connection is a single `Mutex<Connection>` in Tauri managed state (`Db`), so
  every command locks it; don't hold the lock across an `await`.

### Frontend

SvelteKit in SPA mode (`adapter-static`, `ssr = false` in `+layout.ts`) — no server, no load
functions. State is a single **Svelte 5 runes** class instance in `src/lib/stores/app.svelte.ts`,
exported as `app`; components read and mutate its fields directly. `app.init()` is idempotent and
boots from `+page.svelte`.

UI-relevant conventions:

- View switching is `app.view` (`dedup` | `consolidate` | `pathlimits`), branched in
  `+page.svelte`; `VIEWS` in the store drives both sidebar and breadcrumb.
- Anything that should survive a restart goes through `app_state_get`/`app_state_set` (a
  global key/value table): active workspace, current view. Per-workspace settings (dedup
  sliders, `dedup_stale`) instead go through `workspace_state_get`/`workspace_state_set`, keyed
  by `(workspace_id, key)`, so they don't bleed across workspaces.
- Match groups are paged (50/page, infinite scroll). `refreshGroups` resets, `loadMoreGroups`
  appends — filter changes must go through `refreshGroups`.
- Any mutation that invalidates matching sets `dedupStale` so the UI nudges a re-run.

### Styling

Tailwind v4 (CSS-first, no config file) + **shadcn-svelte** with the `nova` style and phosphor
icons — see `components.json`; the `shadcn-svelte` skill in `.claude/skills` covers adding
components. `src/lib/components/ui/**` is generated registry code, so prefer regenerating over
hand-editing.

`src/styles/app.css` defines the design tokens. Beyond the standard shadcn set:
`--brand` (crimson, for text/icons/borders — `--primary` is for solid fills only), and
`--ok` / `--warn` which carry *data* semantics: `--ok` = confident/within limits,
`--warn` = uncertain/getting heavy. `src/lib/util.ts` maps magnitudes onto these
(`dupLevel`, `DUP_BADGE`, `DUP_BAR`, `confidenceTone`) — reuse those helpers rather than
picking colors per component, so a 0.1%-duplicated folder stays visually quiet.
