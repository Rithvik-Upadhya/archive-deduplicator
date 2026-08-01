# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this app does

Archive Deduplicator is a Tauri 2 desktop app for reconciling several offline archives (old discs,
external drives, backup folders) against each other. **It never reads file contents** — it works
purely from filesystem metadata, either imported from a `tree` JSON dump or from a live folder walk.
Everything lives in one local SQLite database, so all state survives a restart.

The user workflow is three views, in order, and much of the backend design follows from it:

1. **Deduplicate** — import sources, run the matcher, browse match groups and per-device stats.
2. **Consolidate** — drag nodes from source trees into a target ("end-state") tree; export the
   plan as a Markdown guide.
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
cargo test                 # Rust tests (currently only dedup.rs)
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
parse.rs / scan.rs  →  nodes table  →  dedup.rs  →  rollup.rs  →  dup_annot cache
   (import)                            (matching)   (folders +      (fast tree browsing)
                                                     device stats)
```

- **`parse.rs`** flattens a `tree -JDs --inodes --device` JSON doc into `FlatNode`s, precomputing
  `subtree_size` / `subtree_file_count` bottom-up so the UI never aggregates. **`scan.rs`** produces
  the same `Flattened` struct from a live `WalkDir`, so both import paths share `insert_nodes`.
- **`dedup.rs`** is the matcher. Exact file size is the bucketing key (that's what survives a
  rename); name, stem, extension, mtime, parent-folder name and inode/dev are folded into a 0–100
  confidence in `score_pair`, then union-find groups the survivors. Two tunables come from the UI:
  `min_size_bytes` (a **hard filter** — small files collide by size constantly) and
  `min_confidence`. inode/dev equality only scores within the same source; across sources it's
  coincidence.
- **`rollup.rs`** builds folder-level groups on top of the file groups, then `rebuild_annotations`
  writes the `dup_annot` table (per-node `has_dup` / `dup_pct`) so `get_tree` is a plain query.
- **`pathfix.rs`** walks the *consolidation* tree, not the sources — but descends into `nodes` for
  subtrees dragged in wholesale. Renames of consolidation nodes are written straight to
  `consolidation_nodes.name`; renames of files inside a dragged-in source directory are stored as
  virtual edits in `pathfix_state` and folded in at path-computation and export time.
- **`dbio.rs`** — export is a SQLite backup-API copy of the live file. Import is deliberately
  non-destructive: it attaches the external file read-only and recreates every workspace found as
  a *new* workspace with all foreign keys remapped. Never make import overwrite.
- **`db.rs`** owns the schema, created idempotently on every launch via `CREATE TABLE IF NOT
  EXISTS`. There is no migration framework — schema changes go in `init_schema`, and any
  incompatible change needs an explicit `DROP`/`ALTER` line there (see the `pathfix_edits` drop).
  The connection is a single `Mutex<Connection>` in Tauri managed state (`Db`), so every command
  locks it; don't hold the lock across an `await`.

`export_action_log` derives the guide from the **current tree**, not from `action_log` history —
so planning moves that cancelled out never show up. Keep it that way.

### Frontend

SvelteKit in SPA mode (`adapter-static`, `ssr = false` in `+layout.ts`) — no server, no load
functions. State is a single **Svelte 5 runes** class instance in `src/lib/stores/app.svelte.ts`,
exported as `app`; components read and mutate its fields directly. `app.init()` is idempotent and
boots from `+page.svelte`.

UI-relevant conventions:

- View switching is `app.view` (`dedup` | `consolidate` | `pathlimits`), branched in
  `+page.svelte`; `VIEWS` in the store drives both sidebar and breadcrumb.
- Anything that should survive a restart goes through `app_state_get`/`app_state_set` (a
  key/value table): active workspace, current view, dedup sliders, `dedup_stale`.
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
