# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this app does

Archive Deduplicator is a Tauri 2 desktop app for reconciling several offline archives (old discs,
external drives, backup folders) against each other. It matches primarily on filesystem metadata,
either imported from a `tree` JSON dump or from a live folder walk, but a live scan can also
opt into **BLAKE3 content hashing** (full or sampled, per-workspace-locked) for its highest-
confidence tiers — imported `tree` JSON sources are still metadata-only, since the JSON dump
never carries file bytes. Everything lives in one local SQLite database, so all state survives
a restart.

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
cargo test                 # Rust tests (db.rs, dedup.rs, links.rs, rollup.rs, dbio.rs, consolidate.rs, pathfix.rs, medium.rs, hashing.rs)
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
parse.rs / scan.rs  →  nodes table  →  links.rs  →  hashing.rs  →  dedup.rs  →  rollup.rs  →  dup_annot cache
   (import)          (medium.rs      (hardlinks)   (content       (matching)   (folders +      (fast tree
                       detects the                   hashing,                   listing hashes   browsing)
                       volume first)                 optional)                  + device stats)
```

- **`parse.rs`** flattens a `tree -JDs --inodes --device` JSON doc into `FlatNode`s, precomputing
  `subtree_size` / `subtree_file_count` bottom-up so the UI never aggregates. **`scan.rs`** produces
  the same `Flattened` struct from a live `WalkDir`, so both import paths share `insert_nodes`.
  Neither module knows about hardlinks — that's `links.rs`'s job, run as a post-`insert_nodes` step.
  `scan.rs` also calls **`medium.rs`** once per scan root to detect storage medium (HDD/SSD/network/
  optical) and filesystem type, persisted onto the `sources` row and used both as the hardlink-trust
  gate (below) and to pick hashing defaults (queue depth, sampling vs. full-hash, `hash_min_size`).
- **`links.rs`** detects hardlink alias sets (files sharing `(dev, inode[, inode_high])` within one
  source) right after `insert_nodes`, inside the same transaction. `inode_high` holds the upper 64
  bits of ReFS's 128-bit file IDs (kept as a separate column rather than folded into one i64, to
  avoid silently colliding two different ReFS files that share a lower 64 bits). Only trusted where
  the volume's filesystem is on `medium::filesystem_is_trusted`'s allowlist (NTFS/ReFS/ext*/XFS/
  Btrfs/APFS/HFS+/ZFS) **and** the source is a live scan — `tree` JSON imports are always untrusted,
  since the dump has no way to attest to real device/inode semantics. The lowest-`rel_path` member
  becomes canonical; the rest get `nodes.alias_of` set and a `kind='hardlink', confidence=100` match
  group. Aliases stay fully visible in `get_tree` (the user still needs to see every name a physical
  file has) but are excluded from `subtree_size`/`subtree_file_count` propagation into ancestors and
  from the matcher's candidate pool (`dedup.rs::load_files` filters `alias_of IS NULL`) — deleting
  `k-1` aliases frees zero bytes until the last name is gone, so they must never be scored as
  ordinary duplicates.
- **`hashing.rs`** is an optional post-`links.rs` pass, run on demand per source via the
  `run_hash_scan` command (its own DB connection, like `run_dedup`, so a multi-hour hash doesn't
  block other commands) — nothing calls it automatically after `scan_folder`, so until the frontend
  adds a trigger for it, hashing only happens if something explicitly invokes the command. BLAKE3, either full-file or sampled (head + tail + interior probes derived
  purely from file size and the workspace's locked `HashSpec`, so region choice is deterministic
  across resumed/re-run scans). A `hash_cache` table keyed by `(volume_id, file_id, size, mtime,
  hash_spec)` makes re-scanning an unchanged tree a no-op read-wise. Resumable: candidates are
  simply every eligible node with `content_hash IS NULL` (not an index-based cursor — that would
  desync as the candidate list shrinks across resumed calls), processed in transactional batches of
  2,000 so a crash loses at most one batch and never leaves a node with a partial hash. The hash
  spec is locked per-workspace (`workspace_state` keys `hash.spec`/`hash.locked`) the first time any
  node in it is actually hashed, so mixing incompatible specs within one workspace is structurally
  prevented rather than merely discouraged.
- **`dedup.rs`** is the matcher: absolute vetoes (different size via bucketing, different extension,
  zero-byte, or — for two files that are both hashed under the same spec — differing digests) followed
  by five fixed-confidence evidence tiers that never merge or average — A (100, full-file hash match),
  B (99, sampled-hash match), C (70, exact name + strong mtime), D (55, exact name only), E (45, same
  parent-folder name + some mtime support, names differ). Tiers A/B are built by plain hash-equality
  grouping (`HashMap` keyed on `(size, hash_spec, content_hash)`) since digest equality is a true
  transitive equivalence relation — the one place this matcher uses union-find-style grouping.
  Metadata tiers C–E are never built that way; a group must be a genuine clique (every pair
  independently qualifies at that tier), enforced via exact-name grouping for C/D (name equality is
  transitive) and Bron–Kerbosch maximal-clique enumeration for E (the mtime-tolerance edge condition
  is not transitive) — and a pair where both sides are hashed is skipped entirely in C–E, since a
  hash match already claimed it at A/B and a hash mismatch is an absolute veto; a hashed file paired
  with an *unhashed* one is untouched and still eligible for ordinary metadata evidence. Two tunables
  come from the UI: `min_size_bytes` (a **hard filter**, default 64 KiB) and `min_confidence` (a tier
  cutoff now, not a continuous threshold). `kind='hardlink'` groups are `links.rs`'s, not this
  module's — the group-clearing `DELETE` at the start of a run explicitly spares them.
- **`rollup.rs`** builds folder-level groups on top of the file groups via two independent
  detectors: the original 80%-byte-overlap heuristic, and a Merkle-style bottom-up `listing_hash`
  (BLAKE3 over each directory's sorted immediate children — `"file:{name}:{size}:{mtime_norm}"` or
  `"dir:{name}:{child.listing_hash}"` — so structurally identical subtrees hash equal even when
  renamed, or when dominated by small unhashed files). Directories sharing a `listing_hash` across
  ≥2 sources get a `kind='folder', primary_signal='listing', confidence=60.0` group alongside (not
  replacing) the byte-heuristic's groups; both can legitimately fire on the same directory pair.
  `rebuild_annotations` then writes the `dup_annot` table (per-node `has_dup` / `dup_pct`) so
  `get_tree` is a plain query. Its byte/leaf rollup queries (`load_file_locs`, `load_leaf_locs`)
  also filter `alias_of IS NULL`.
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
  helps the former. Current `SCHEMA_VERSION = 3`, which added hashing/medium columns to `nodes` and
  `sources` plus two brand-new tables, `hash_cache` and `scan_progress` (new tables need only the
  `CREATE TABLE IF NOT EXISTS` in `init_schema`, not a `migrate` entry). The connection is a single
  `Mutex<Connection>` in Tauri managed state (`Db`), so every command locks it; don't hold the lock
  across an `await` — `run_dedup` and `run_hash_scan` both open their own dedicated connection via
  `db::open` instead, so a long pass doesn't block the rest of the UI.

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
