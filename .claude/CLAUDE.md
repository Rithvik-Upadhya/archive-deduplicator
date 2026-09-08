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
3. **Fix Paths** — walk the _consolidated end-state_ tree for paths over the Windows 260-char
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
- pnpm 11 runs a deps-status check before _every_ script, so `pnpm check`, `pnpm dev`, and
  `pnpm tauri dev` all trigger the same implicit `pnpm install` and hit the same wall. There is no
  read-only pnpm subcommand that dodges it.

Running the app from a sandbox is not a workaround either: Tauri's Linux stack (webkit2gtk-4.1,
GTK3, libsoup-3.0) is absent, and there is no display, X server, or Xvfb. `pnpm tauri dev` is a
**macOS-host-only** command.

### Branches, not worktrees

**Claude: never create a git worktree in this repo — use an ordinary branch.** That means no
`git worktree add` and no `EnterWorktree`; `git checkout -b <name>` from `main` instead. Work is
merged back into `main` directly and the branch deleted; this repo does not use pull requests, and nothing is pushed to `origin`. **And when the change is confined to one or two files, always make the changes directly on main unless otherwise instructed.** Do not litter the git history with unnecessary branches for small changes. If the change breaks something we can always roll back to a working commit.

Worktrees cost more than they're worth here: `src-tauri/target/` is not shared, so every worktree
triggers a full multi-minute rebuild of the entire Tauri dependency tree (and the link step alone
needs more RAM than a sandbox typically has). A worktree also can't merge into `main` while the
main checkout has it checked out, so the work strands until someone merges it by hand, and the
stray directory plus its branch then have to be cleaned up separately.

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
  the volume's filesystem is on `medium::filesystem_is_trusted`'s allowlist (NTFS/ReFS/ext\*/XFS/
  Btrfs/APFS/HFS+/ZFS) **and** the source is a live scan — `tree` JSON imports are always untrusted,
  since the dump has no way to attest to real device/inode semantics. The lowest-`rel_path` member
  becomes canonical; the rest get `nodes.alias_of` set and a `kind='hardlink', confidence=100` match
  group. Aliases stay fully visible in `get_tree` (the user still needs to see every name a physical
  file has) but are excluded from `subtree_size`/`subtree_file_count` propagation into ancestors and
  from the matcher's candidate pool (`dedup.rs::load_files` filters `alias_of IS NULL`) — deleting
  `k-1` aliases frees zero bytes until the last name is gone, so they must never be scored as
  ordinary duplicates.

  **Counts count names; sizes count bytes held.** These are two different axes and mixing them is
  the single most productive source of bugs in this codebase.

  A **count** counts *names*: every canonical file, hardlink alias and symlink. Someone listing the
  folder by hand must not find a different number of files than the app reported. So `file_count`,
  `subtree_file_count` and `cross_dup_file_count` all include aliases and symlinks, and
  `links.rs::recompute_subtree_totals` propagates an alias's count into its ancestors.

  A **size** counts *bytes actually held*: an alias is a second name for bytes the canonical already
  contributed, and a symlink has no content of its own, so neither adds to `total_size`,
  `subtree_size`, `physical_size` or `cross_dup_size`. Deleting `k-1` aliases frees nothing.

  Hence the deliberate asymmetry in `rollup.rs`: an alias bumps `cross_dup_file_count` and **not**
  `cross_dup_size`. That is not an oversight to tidy into symmetry — since `subtree_size` excludes
  alias bytes too, adding them to `cross_dup_size` would make `subtree_size - cross_dup_size`
  subtract bytes that were never in the total.

  Anything derived from `match_members` is canonical-only, because aliases never reach the matcher,
  so a **byte** figure from there must be divided by an alias-free base — `duplicated_pct` uses
  `physical_size`, and dividing by `total_size` instead capped a byte-for-byte duplicate device at
  63%. Note this trap is *latent*: that divide was correct when written and only became wrong once
  aliases were later excluded from the candidate pool.

  For **counts** the matcher's canonical-only output must instead be widened back out: an alias
  inherits its canonical's `cross_dup` in `rebuild_annotations_with`. Without that an alias has no
  `dup_annot` row at all, every `COALESCE(cross_dup, 0)` reads it as exclusive-to-this-device, and a
  filtered consolidation drag carries across a second name for content the filter just hid.
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
  is not transitive). Two hash-derived exclusions then apply to C–E: a pair hashed under the same
  spec with _differing_ digests is an absolute veto (it must never be softened into a metadata
  match), and — separately — a metadata group whose members _all_ belong to one and the same A/B
  hash group is dropped as redundant, since it only restates at lower confidence a group content
  evidence already produced. That second rule is deliberately whole-group rather than per-pair: a
  _mixed_ group (some members hash-mates, some not) is emitted intact, because suppressing the
  individual hash-mate pairs would split it into one group per cross-cohort pairing and inflate the
  count this rule exists to reduce. A hashed file paired with an _unhashed_ one is untouched and
  still eligible for ordinary metadata evidence. Two tunables
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
  The per-source stat helpers `source_list` calls — `duplicated_size_by_source` and
  `cross_dup_size_by_source` — each keep a `counted: HashMap<i64, HashSet<i64>>` guard, because a
  file may sit in more than one group across tiers and billing its bytes once per group lets a
  source report more duplication than it physically holds.
- **`pathfix.rs`** walks the _consolidation_ tree, not the sources — but descends into `nodes` for
  subtrees dragged in wholesale. Renames of consolidation nodes are written straight to
  `consolidation_nodes.name`; renames of files inside a dragged-in source directory are stored as
  virtual edits in `pathfix_state` and folded in at path-computation and export time.
- **`dbio.rs`** — export is a SQLite backup-API copy of the live file. Import is deliberately
  non-destructive: it attaches the external file read-only and recreates every workspace found as
  a _new_ workspace with all foreign keys remapped (including `nodes.alias_of`, which — unlike
  `parent_id` — can reference a node with a _lower_ id than itself, since a hardlink's canonical is
  chosen by `rel_path` rather than insertion order; it's backfilled in a second pass once every id
  is mapped). Never make import overwrite.
- **`db.rs`** owns the schema (`init_schema`, `CREATE TABLE IF NOT EXISTS`, safe for brand-new
  databases) plus a real migration step (`migrate`, gated on `PRAGMA user_version` so it stays a
  read-only no-op once a database is current — see the doc comment on why an unconditional write
  there would reintroduce lock contention with `run_dedup`'s dedicated connection). Adding a column
  to an already-shipped table needs an entry in _both_ `init_schema`'s DDL (for fresh databases) and
  `migrate`'s `alterations` list (for existing ones) — `CREATE TABLE IF NOT EXISTS` alone only ever
  helps the former. `SCHEMA_VERSION` is **9**; version 3 added the hashing/medium columns to `nodes`
  and `sources` plus two brand-new tables, `hash_cache` and `scan_progress` (new tables need only the
  `CREATE TABLE IF NOT EXISTS` in `init_schema`, not a `migrate` entry). Note that a column added
  by `migrate` is **not backfilled** — existing rows get the `DEFAULT`. `sources.physical_size`
  arrived that way, so pre-migration sources still carry 0 and every reader needs a fallback.
  Version 9 is the one migration that is *not* a column add: it recomputes `sources.file_count` and
  `nodes.subtree_file_count` for every existing source, because those changed meaning (counts became
  name-based) and stale numbers cannot be defaulted away. It reuses
  `links::recompute_subtree_totals` and stays behind the `user_version` early-return. The connection is a single
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
`--ok` / `--warn` which carry _data_ semantics: `--ok` = confident/within limits,
`--warn` = uncertain/getting heavy. `src/lib/util.ts` maps magnitudes onto these
(`dupLevel`, `DUP_BADGE`, `DUP_BAR`, `confidenceTone`) — reuse those helpers rather than
picking colors per component, so a 0.1%-duplicated folder stays visually quiet.
