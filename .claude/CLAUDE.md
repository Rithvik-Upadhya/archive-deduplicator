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
cargo test                 # Rust tests (db.rs, dedup.rs, links.rs, rollup.rs, dbio.rs, consolidate.rs, pathfix.rs, medium.rs, hashing.rs, search.rs)
cargo test detects_cross_source_file_duplicates    # single test
cargo clippy && cargo fmt
```

There is no JS test runner and no ESLint config; `pnpm check` is the only frontend gate.
Formatting is Prettier via `.prettierrc` (4 spaces, single quotes, `bracketSameLine: true`).

### Running pnpm and cargo

This repo now runs in a Linux dev environment (not the old macOS-host sandbox), so `pnpm install`
and the other `pnpm`/`cargo` commands are safe to run here.

**Claude: do not run `pnpm` or `cargo` commands unless the user explicitly asks you to.** Backend
and frontend changes still ship unverified unless the user requests a build/test/check run.

The request has to be a live one, made in the moment. In particular, a plan that lists a build,
test, typecheck or lint step is **not** that request, even once the plan is approved: approving a
plan approves the code changes it describes, not the commands. Finish the edits, then say which
command would verify them and let the user ask for it. The same goes for a step you inferred
yourself — "I changed Rust, so I should `cargo test`" is exactly the automatic run this rule
forbids. `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt`, `pnpm check`, `pnpm install`
and `pnpm tauri build` all count, including when they are only a step on the way to something the
user did ask for.

If `pnpm` or `cargo` is not on `PATH` at the start of a session, `source /home/agent/.bashrc` and
retry once. If that still doesn't find them, stop and report to the user, and ask how to proceed —
do not try to fix the environment yourself.

Running the full app (`pnpm tauri dev`) still needs Tauri's Linux stack (webkit2gtk-4.1, GTK3,
libsoup-3.0) and a display; if those are absent it remains a macOS-host-only command.

### Branches, not worktrees

**Claude: never create a git worktree in this repo — use an ordinary branch.** That means no
`git worktree add` and no `EnterWorktree`; `git checkout -b <name>` from `main` instead. Work is
merged back into `main` directly and the branch deleted; this repo does not use pull requests, and nothing is pushed to `origin`. **And when the change is confined to one or two files, always make the changes directly on main unless otherwise instructed.** Do not litter the git history with unnecessary branches for small changes. If the change breaks something we can always roll back to a working commit.

**Claude: only commit when the user explicitly instructs you to.** Make and stage changes, but
leave committing to an explicit request.

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
  file has) but are excluded from `subtree_size` propagation into ancestors (their *counts* still
  propagate — see below) and
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
  or — for two files that are both hashed under the same spec — differing digests) followed
  by six fixed-confidence evidence tiers that never merge or average — A (100, full-file hash match),
  B (99, sampled-hash match), **S (100, two symlinks with the same name and the same `link_target`)**,
  C (70, exact name + strong mtime), D (55, exact name only), E (45, same
  parent-folder name + some mtime support, names differ).

  **Only `min_size_bytes` excludes a node from matching.** That setting is the user's
  processing-time budget — they trade completeness for speed knowingly — whereas a category
  hardcoded out of the candidate pool is an exclusion they never chose and cannot see. Two used to
  be: symlinks (`load_files` filtered `type = 'file'`) and zero-byte files (an absolute veto). Both
  could therefore never be `cross_dup`, so the "exclusive to this device" funnel asserted that every
  symlink and every empty file was unique to its source, which is what it means for a filter to lie.
  Both now participate, subject only to the threshold, and carry their own vetoes instead:

  - A **symlink** is grouped on `(name, link_target)` — a target *is* the node's whole content, so
    equality there is an equivalence relation (grouped by `HashMap`, like the hash tiers, never by
    clique search) and a differing target simply fails to match. Expressing the veto as the grouping
    key also makes the type veto structural: symlinks never enter the metadata size-buckets, so one
    can never be grouped with a same-sized regular file.
  - A **zero-byte file** reaches tiers C/D (exact name) but is excluded from tier E. "Same name,
    empty on both sources" is evidence; "different names, same parent folder, similar mtime, both
    empty" is coincidence — there is no content to agree. This is what survives of the old blanket
    veto, and it keeps every empty file in the workspace (one size bucket, one extension sub-bucket,
    often one mtime) out of the clique search that `MAX_CLIQUE_CANDIDATES` exists to bound.
    `hashing.rs` also refuses them regardless of `hash_min_size`: BLAKE3 of no bytes is a constant,
    so hashing them would put every empty file in one tier-A group at confidence 100. Tiers A/B are built by plain hash-equality
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
  ≥2 sources get a `kind='folder', primary_signal='listing', confidence=70.0` group alongside (not
  replacing) the byte-heuristic's groups; both can legitimately fire on the same directory pair.

  **Folder confidence measures evidence, not coverage.**
  - The 80% byte test decides *whether* a folder is reported. The group's confidence is the
    byte-weighted mean of each duplicated file's best cross-source confidence, so a folder resting
    on tier-E matches reads about 45, not 100. Coverage lives in `dup_pct`. It used to be the score
    (80–100), which put a tier-E-backed folder level with a hash match and made `min_confidence`
    meaningless for folders.
  - `listing` sits at tier C (70), because every file in a matching subtree meets C-grade evidence.
    To keep trivial structure out, a folder only seeds a group when its subtree holds ≥ 2 names and
    ≥ `min_size_bytes` of file data; otherwise every `desktop.ini`-only folder would match every
    other.
  - Both folder kinds store the largest member's subtree bytes as `size`. `listing` groups used to
    store 0, which the group list's `size >= min_size` filter hid under the default 64 KB.
  `rebuild_annotations` then writes the `dup_annot` table (per-node `has_dup` / `dup_pct`) so
  `get_tree` is a plain query. Its byte/leaf rollup queries (`load_file_locs`, `load_leaf_locs`)
  also filter `alias_of IS NULL`.
  **A file can be in more than one match group**, and every consumer has to handle that. It is the
  single most repeated bug in this file. `duplicated_size_by_source` keeps a
  `counted: HashMap<i64, HashSet<i64>>` guard, because billing a file's bytes once per group lets a
  source report more duplication than it physically holds. And cross-source-ness must be read as
  "**any** of this file's groups spans >1 source" — hence `load_cross_source_files`, which the
  folder rollup, `dup_annot.cross_dup` and `cross_dup_size_by_source` all take rather than deriving
  themselves. It returns a `CrossSource`: membership, plus each file's *highest* cross-source group
  confidence (the folder rollup's weight), both from the same pass. Never rebuild it from a `HashMap<node_id, group_id>`: collapsing a file to one
  arbitrary group, and then deriving each group's source set from that collapsed map, silently
  loses cross-ness from both ends and marks genuine duplicates exclusive-to-this-device (it was
  mislabelling 2,562 of 30,696 files, with no false positives to make it visible).

  That helper also folds in the hardlink aliases, once, so the device header and the tree cannot
  disagree about what the funnel hides — when `cross_dup_size_by_source` computed its own answer it
  missed them and reported 5,071 exclusive files where the tree showed 1,322.

  **`dup_annot` gets a row for every directory, and for skipped files** — not only for nodes with
  duplicated bytes. `get_tree` reads it through `COALESCE(…, 0)`, so a *missing* row silently means
  "not duplicated, not hidden, not skipped", which is a claim rather than an absence. That is how
  153 empty directories stayed visible under the funnel reporting "0 files", and it is why
  `dir_cross_dup` has no `total > 0` guard: a directory with no leaves is vacuously fully-hidden.

  **A safety cap is not a finding.** When `dedup.rs` declines to judge a cohort (oversized name
  group, un-enumerable clique graph, internally contradictory group) its members are marked
  `dup_annot.skipped`, rolled up to ancestors as `skipped_count`, and badged in the tree. Without
  that, 2,618 cargo build artifacts read as "exclusive to this device" when the truth was "never
  compared". Only the `>` cap branches mark it — a `< 2` branch means no partner exists, which is
  genuine uniqueness, and conflating the two would badge every unique file in the workspace.
- **`pathfix.rs`** walks the _consolidation_ tree, not the sources — but descends into `nodes` for
  subtrees dragged in wholesale. Renames of consolidation nodes are written straight to
  `consolidation_nodes.name`; renames of files inside a dragged-in source directory are stored as
  virtual edits in `pathfix_state` and folded in at path-computation and export time.
  **`pathfix::rename` (the `pathfix_rename` command) is the only rename path for both the
  Consolidate and Fix Paths trees.** It records the original once in `pathfix_state`, so a rename
  made in either view can be reset from either. Renaming a row back to its recorded original
  counts as a revert: it deletes the row instead of leaving an edit that changes nothing.
  `ConsolidationNode.original_name` is read from that table, and `Some` means "renamed".
- **After-name indicators** (`NameMarks.svelte`, shared by both tree rows):
  - a reset button when the row itself is renamed;
  - a `textbox` mark with a count when something beneath it is renamed;
  - a strikethrough mark when something beneath the row is struck but the row isn't;
  - an `arrows-out-cardinal` mark for a **move within one source**: in the source's colour on the
    moved row, muted on every folder above it (the count is in its tooltip).

  The "beneath" counts cover every descendant and come from `util.countBelow`; don't derive them
  separately per view — that includes the moved-item counts. Relocation itself comes from
  `util.relocationsFor`, also shared, and is derived from positions rather than move history: a
  row is relocated when its end-state parent is from the same source but isn't the folder its
  `origin_path` sits in. A parent from another source or a hand-made folder is not an internal
  move — the source bars already show those.
- **Consolidated-tree marks.** `consolidation_nodes.done` ("carried out on disk") and `struck`
  ("to be deleted on disk") are independent flags. `done` applies to one row only. `struck` is
  stored only where it was applied and **inherited downward at read time**: a row is struck when it
  or any ancestor is. A struck subtree is not part of the end state, so the consolidated totals
  count it only toward struck ancestors. `pathfix.rs::walk_cons` **shows it but never measures
  it**:
  - `PathTreeNode.struck` is the inherited value;
  - a struck row is never `over_limit` and never makes an ancestor so;
  - a live folder whose children are all struck is an end-state leaf, so it is measured itself.
    The frontend's badge and `overCount` use the same "live, with no live children" rule.

  A hard removal goes through `consolidation_delete_nodes`, which takes the selection's roots, and
  **Trash exists only in the Consolidate tree**.
  - **Colour.** A row is green (`--done`) only when it **and every descendant** is done; a
    ticked folder with outstanding contents keeps the tick icon but has no tint. Otherwise a
    struck row is red (`--struck`). "Fully done" beats struck, so a finished deletion is green
    and keeps its strikethrough.
  - **Fix Paths** does not share the Consolidate tree's UI. Its toolbar is **Folder** (the
    shared `NewFolderDialog`) plus the **Strikethrough** toggle. Both views' toggles use
    `util.strikeToggle`. It has no done marks and no trash.
  - **In both trees, a row background means *selected* and nothing else** (`bg-muted/50`).
    Folders carry no tint and over-limit rows only a red border, so don't reintroduce either as
    a fill.
- **Tree selection is hierarchical** (`stores/selection.svelte.ts`, shared by the device,
  consolidated and Fix Paths trees). `selected` holds only roots, and a row is selected when it or
  any ancestor is a root. Ctrl/cmd-click on a child of a selected folder does nothing, and adding a
  folder absorbs the roots beneath it. Operations consume `roots()` or `dragIds()`, so no item is
  reached by two routes. Rows must pass `parentId` to `use:selectable`. The Deduplicate view's
  device trees are selectable too (not draggable): a row click selects, and only the caret
  expands.
- **`consolidate.rs`** also owns `purge_source_files`, which `source_delete` must call **before**
  deleting the source row: `consolidation_nodes.source_node_id` is `ON DELETE SET NULL`, so once
  the `sources → nodes` cascade runs there is nothing left to say which consolidation rows came
  from that source, and they survive as ghosts that still count as files while reporting no size
  and no origin. It removes that source's *files* only — folders are deliberately kept, since
  working out whether the user has cross-populated one with another source's files costs more than
  a stray empty folder is worth. Pre-existing ghosts from before this fix are not migrated away;
  every reader already coalesces a NULL `source_node_id` to 0 bytes / no origin.
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
  helps the former. `SCHEMA_VERSION` is **12** (v11: `sources.color`, the user-picked source colour; v12: `consolidation_nodes.done`/`struck`, the archivist's progress marks); version 3 added the hashing/medium columns to `nodes`
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
  tuning fields, `dedup_stale`) instead go through `workspace_state_get`/`workspace_state_set`, keyed
  by `(workspace_id, key)`, so they don't bleed across workspaces.
- Match groups are paged (50/page, infinite scroll). `refreshGroups` resets, `loadMoreGroups`
  appends — filter changes must go through `refreshGroups`.
- Any mutation that invalidates matching sets `dedupStale` so the UI nudges a re-run.
- Both end-state trees (Consolidate and Fix Paths) order siblings with `util.compareTreeRows`:
  folders first, then natural, case-insensitive name order. `consolidation_nodes.sort_order` is
  no longer the display order; it survives only as the append position for moves and for
  `pathfix.rs`'s walk.
- The "Locate duplicates" button on device-tree rows opens `GroupDialog` in both views; the
  dialog fetches the group itself and renders it through `GroupMembers.svelte`.
- Every match-group member row (the dialog and the Deduplicate view's group list) ends with an
  arrow, `RevealButton.svelte`, that selects the member in its device tree and scrolls to it
  without switching views. It goes through `revealInDeviceTree` (`stores/treeExpansion`), which
  asks the `node_location` command for the ancestor ids, clears the device's funnel **only** when
  that command says the node is hidden by it, opens the device panel, expands the ancestors, and
  sets `deviceReveal.target`. The target's `TreeItem` scrolls and selects itself when its row
  mounts, which in a lazily loaded tree can be several fetches later.
- **Name search** (`SearchBar.svelte` in the top bar, hidden in Fix Paths; state in
  `stores/search.svelte.ts`, driven by `app.runSearch`/`app.clearSearch`). A substring match on a
  node's *name*, never its path, and it runs only on Enter or the search button. `*` (any run,
  including none) and `?` (one character) are wildcards *inside* that substring match — unanchored,
  so `IMG_*.jpg` also matches `old-IMG_1.jpg.bak` — with no escape for a literal `*`/`?`. `search.applied`
  is the search in effect, and `query`/`caseSensitive` are only what the input holds until then.
  Switching views or workspaces clears it.
  - The device trees and the group list load lazily, so the backend matches: `search_nodes` returns
    matches plus ancestors, and `get_groups` takes `search`/`caseSensitive`. A group is kept when any
    member on a non-excluded source matches. The consolidated tree is filtered in
    `ConsolidationView` from memory; its unfiltered `childrenOf` still drives drops and sort orders.
  - The comparison is `search.rs::contains_folded`/`glob_contains`, exposed to SQL as `search_match`
    (SQLite's `lower()`/`LIKE` fold ASCII only), and `util.compileNameQuery` on the frontend. The
    store compiles it once as `search.compiled`, and the highlight, the matched-member label and the
    consolidated tree's filter (`search.matches`) all go through it. Both sides lowercase in full
    Unicode (never the regex `i` flag), and `?` takes one `char` / one code point (`u` flag). Change
    them together.
  - While searching, trees expand from `search.deviceExpanded`/`consExpanded`, not the normal sets,
    so clearing restores the old expansion. Ancestors are auto-opened only up to
    `AUTO_EXPAND_LIMIT` matches; the filter itself is never capped. Reveal arrows are disabled,
    and view selections clear on every search change.
- **Icons go through `$lib/components/Icon.svelte`. Never import `@iconify/svelte`.** That package
  ships no icon data — it fetches every glyph from `api.iconify.design` at runtime, so all icons
  silently render empty offline, which is the one condition this app exists for. The wrapper maps a
  `ph:*` name onto a bundled `phosphor-svelte` component (weight parsed from the `-fill`/`-bold`
  suffix), keeping the `icon="ph:name"` string API, so names can still live in plain data like
  `VIEWS` and `TASK_STATUS_ICON`. Adding an icon means adding its base name to that component's
  `BASE` map; `IconName` is derived from the map, so `pnpm check` fails on a name that is used but
  not bundled rather than letting it disappear at runtime. Note `phosphor-svelte@3.1.0` has no
  `Radar`, hence `ph:scan-fill` on the Fix Paths scan button.
- Copying a path — a file's or a folder's — uses `copyPath` in `src/lib/util.ts`
  (`navigator.clipboard` — the Tauri webview's custom protocol is a secure context, so no clipboard
  plugin is needed) joined on `app.pathSep`, which `init()` fills from the `path_separator` command.
  Every path it is given is already free of a device/source name: `nodes.rel_path` is relative to
  the scan root, and the consolidation/pathfix `pathOf` walks stop at a user-made root. It returns
  a success boolean that no caller currently checks, so a clipboard failure is silent today.
- **Hover text and copy text deliberately differ in the consolidation and Fix Paths trees.** Both
  rows show `device://source-path` on hover (`origin`, built from `origin_device`/`origin_path`)
  while their copy button yields the source-free end-state path from `pathOf`. Do not "fix" them
  into agreement — a user pasting into a file manager wants a usable path, but a user hovering
  wants to know which shelf the thing came off. Note this means a Fix Paths row can show a source
  path in its tooltip while the badge beside it counts the *end-state* path length; that is two
  different paths in one row, on purpose. `pathfix.rs::load_cnodes` gets the origin from the same
  two `LEFT JOIN`s `consolidation_get` uses, and they must stay `LEFT`: a hand-created folder has
  `source_node_id IS NULL` and an inner join would drop it from the tree.

### Styling

Tailwind v4 (CSS-first, no config file) + **shadcn-svelte** with the `nova` style and phosphor
icons — see `components.json`; the `shadcn-svelte` skill in `.claude/skills` covers adding
components. `src/lib/components/ui/**` is generated registry code, so prefer regenerating over
hand-editing.

`src/styles/app.css` defines the design tokens. Beyond the standard shadcn set:
`--brand` (crimson, for text/icons/borders — `--primary` is for solid fills only), and
`--ok` / `--warn` which carry _data_ semantics: `--ok` = confident/within limits,
`--warn` = uncertain/getting heavy. `confidenceTone` in `src/lib/util.ts` maps a match
confidence onto those — reuse it rather than picking colors per component.

**Duplicate markers are coloured by _where the other copies live_, never by how much.**
`DUP_TONE` (`src/lib/util.ts`) is the only source of those classes: `external` (brand red) means
the item also exists on another source, `internal` (muted grey) means every copy is on this one
device. "How much" is already the number printed on the badge; what the user decides on is
whether deleting something would lose their only copy. So:

- a **file** badge is red when `cross_dup`, grey otherwise;
- a **folder** badge is red when `cross_dup_file_count > 0` — i.e. *any* name beneath it has a copy
  on another source. Deliberately a count, **not** the directory's own `cross_dup`: that flag is
  all-or-nothing (`dir_cross_dup` requires *every* leaf to be cross-source), so a folder of
  ninety-nine unique files and one duplicate rendered grey, asserting "all exclusive to this
  device" about the one file the user needed to see. The count is also rolled up the whole ancestor
  chain, so one deeply nested duplicate reddens every folder above it, and it counts *names* —
  aliases and symlinks included — which is the right axis for "even one file". A further benefit:
  it drops `cross_dup`'s vacuous truth for a leaf-less directory (`0 == 0`, unguarded), so an empty
  folder is grey rather than red.
- that count is in the folder badge's **visibility** gate as well as its tone. `dup_pct` is a share
  of bytes, so a folder whose only cross-source entries are symlinks, hardlink aliases or empty
  files has none — exactly the folders the count rule exists to catch. Such a badge prints a red
  `0%`, which is honest for a byte-share column. Every folder badge's tooltip states what the number
  is a share *of* — and states plainly that `dup_pct` pools duplication of **both** kinds, since the
  tone beside it is about cross-device only. One row, two questions; do not let the tooltip imply
  the percentage is the cross-device share. The extra note explaining a `0%` reading appears **only
  when the displayed figure is actually 0**, and distinguishes the two ways it can be: the
  duplicates hold no bytes at all, or they hold so few that one decimal place rounds them away.
  Those are different facts and the note must not claim the first when it is the second.
- `in_folder_group` is no longer part of any colour rule but is **still live**: it gates the
  "Locate duplicates" button, which needs a real folder-kind match group to open. Do not remove it
  as dead.
- per-source `% dup` badges carry no tint at all; the split bar beneath them does that job.

`--hit` / `--hit-text` are the search-match yellows. The highlighted text keeps its own colour, so
`--hit` is chosen per theme for that text: bright under near-black, dark mustard under near-white.
They are not `--warn`.

`--warn` is deliberately **not** used for duplication, so a yellow mark always means "not judged"
(skipped) rather than "somewhat duplicated". The older magnitude scale (`dupLevel`/`DUP_BADGE`/
`DUP_BAR`) was removed with this change — don't reintroduce it.

The source bar's two segments come from `duplicated_pct` and `cross_duplicated_pct`, which
`rollup::duplicated_size_by_source` splits in **one** pass under **one** `counted` guard, so
`internal + cross` is exactly the pooled total the badge shows. Do not compute the internal share
from `cross_dup_size` instead: that is the _funnel's_ figure over a different population (it folds
in hardlink aliases and ranges over `nodes` rather than `match_members`), and subtracting it can go
negative.

**Every per-source *byte* figure is files-only.** `duplicated_size_by_source` ranges over
`match_members`, and symlinks reach it — they are matchable via tier S — so it explicitly zeroes
their contribution, the same guard `cross_dup_size_by_source` uses. `nodes.size` for a symlink is
the length of its target path, and `total_size`/`physical_size` are files-only, so billing those
bytes put them in a numerator whose base could not hold them. Hardlink aliases need no such guard:
they never reach the matcher (`load_files` filters `alias_of IS NULL`) and their own groups are
`kind='hardlink'`, which that query excludes. The guard is applied to the *bytes*, not as a SQL
row filter, so a group's source set stays complete — filtering rows would silently change
cross-source determination if a group ever mixed symlinks with files.

**Every per-source duplication figure filters `s.excluded = 0`.** `duplicated_size_by_source` was
the odd one out until it was fixed, and the mismatch was not merely cosmetic: a group whose only
other member sat on an excluded device still read as cross-source there, so the badge counted
redundancy against a device the user had switched off while the funnel beside it treated the same
file as having no partner. When you add a query that decides whether a file "has a duplicate
elsewhere", filter excluded sources in it, or it will disagree with the three that do.

## Recurring failure modes

Every bug found in this codebase so far has been one of the following, and none of them were
crashes — each produced a plausible-looking number that was quietly wrong. The matcher's own
output has been reliable throughout; what fails is the arithmetic and the claims layered on top
of it. Read this before changing anything that produces a number the UI shows.

### 1. Two numbers, one base

Every subtraction and every division needs both sides measured over the *same population*.
Getting this wrong never looks like an error — it looks like a number.

- `duplicated_pct` divided a canonical-only numerator by `total_size`, which counts hardlink
  aliases. The ratio was structurally capped at `physical_size / total_size`, so a byte-for-byte
  duplicate device reported **63%** and no amount of real duplication could lift it.
- The device header subtracted a canonical-only `cross_dup_file_count` from a name-based
  `file_count`, leaving every hidden alias in the visible total — **5,071** shown where the tree
  showed **1,322**.
- A label counts too: `(-3.3 GB hardlinked)` sat beside a size those bytes had *already* been
  removed from, inviting the reader to subtract twice.

**Check:** for every `a - b` and `a / b`, name the population each side ranges over and say them
out loud. Counts count names (files, aliases, symlinks); sizes count bytes actually held. An
alias is +1 name and +0 bytes, which is why `cross_dup_file_count` includes aliases and
`cross_dup_size` must not — that asymmetry is the model, not an oversight to tidy away.

### 2. Correct when written, wrong later

`dup / total_size` was right the day it was written. Five days later hardlink aliases were
excluded from the matcher's candidate pool, and that commit — which never touched the divide —
made it wrong. Nobody wrote a bad line; the meaning of `total_size` shifted underneath a good one.

**Check:** when you narrow or widen a *population* (a `WHERE` clause, a candidate pool, a filter
in a loader), grep for every consumer of quantities derived from it. Ask not "does this still
compile" but "does this still mean what its readers think it means".

### 3. A missing row is a positive claim

`get_tree` reads `dup_annot` through `COALESCE(d.cross_dup, 0)`. A row that was never written
therefore asserts *not hidden, not duplicated, not skipped* — an assertion, not an absence.

- Hardlink aliases got no `dup_annot` row (the annotation pass builds from `load_file_locs`,
  which filters them out), so every alias read as exclusive-to-this-device — and a filtered
  consolidation drag carried across **1,139** aliases whose canonical the same filter had hidden.
- Empty directories got no row either, so **153** of them survived the funnel forever, reporting
  "0 files".
- Skipped files are in no group, so they too would have had no row.

**Check:** for every `LEFT JOIN` + `COALESCE` default, ask what that default *claims* and whether
it is true for the rows deliberately absent from the source table. If not, write rows for them.

### 4. Absence of evidence is not evidence of absence

A filter that promises "files unique to this source" must not report as unique anything it never
examined. Three shapes of this:

- **Safety caps.** `MAX_GROUP_SIZE` declining an oversized cohort is correct — 1,906 same-named
  48-byte cargo timestamps are not one duplicate set — but its output was reported as uniqueness.
  Of 2,656 files the funnel called unique, only **38** were. Hence `dup_annot.skipped`.
- **Structural exclusions.** `load_files` filtered `type = 'file'` and vetoed zero-byte files, so
  822 nodes could never be matched and were always shown as unique.
- **Who chose it.** `min_size_bytes` is the user's processing-time budget: a duplicate missed
  because of it is *their decision*, knowingly made. A category hardcoded out of the pipeline is
  an exclusion they never made and cannot see. The first is fine; the second is a lie in the UI.

**Check:** three states, not two — *duplicated*, *unique*, and *not judged*. Any `continue` in a
matching path is potentially the third. Note that a `< 2` guard means "no partner exists", which
is a genuine finding; only the `>` cap branches are declined judgements. Conflating them would
badge every unique file and make the marker worthless.

### 5. A many-to-many relation flattened into a map

A file legitimately belongs to several match groups across tiers. `HashMap<node_id, group_id>`
filled with `insert` keeps whichever row came last, and building each group's source set *from
that collapsed map* compounds it — a genuine cross-source pair then reads as single-source from
both ends. This mislabelled **2,562 of 30,696** files, with zero false positives to make it
visible.

**Check:** before keying a map on an entity id, confirm the relation is actually 1:1. If it is
not, key on the pair or collect a `Vec`, and derive aggregates from the raw rows rather than from
a lossy index.

### 6. The same question answered in two places

The device header and the tree have now drifted **three times** — the multi-group bug, the alias
inheritance, the cross-dup count — because each derived cross-source-ness independently. Two
correct-looking implementations of one question will diverge the moment either is updated alone.

**Check:** if two paths answer the same question, make one call the other.
`load_cross_source_files` exists for exactly this; add to it rather than beside it.

### 7. Tests that cannot fail, and verification that lies

- The whole suite passed while `cross_dup` was wrong for 2,562 files, because **every test put a
  file in exactly one group** — the one case that always worked.
- `symlink_keeps_directory_visible_and_is_never_annotated` kept passing after symlinks became
  matchable, but only because its symlink had no partner. Passing for the wrong reason.
- A draft test for tier E never reached tier E: its fixture had no parent directory, so the code
  under test was unreachable.
- A verification script reported "8,319 rows lost" that were entirely directories and aliases —
  two categories the query wasn't scoped to.

**Check:** confirm a new regression test *fails before the fix*. Ask what fixture shape the bug
needs and whether any existing test has it. When a verification number surprises you, break it
down by category before believing or acting on it — in both directions.

### Working notes

- `cargo` and `pnpm` are available here but must not be run unless the user asks (see above), so
  backend changes still ship unverified by a compiler unless a run is requested. Grep every struct
  literal after adding a field, and every call site after changing a signature — a text edit that
  matches 2 of 3 sites looks like it worked.
- The reference export under `.references/exports/` is the fastest oracle in this repo: replaying
  a proposed algorithm against it in Python has caught wrong diagnoses (`MAX_CLIQUE_CANDIDATES`
  vs `MAX_GROUP_SIZE`) and confirmed fixes before any Rust compiled. Prefer it over reasoning.
