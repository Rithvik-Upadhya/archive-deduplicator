//! Content hashing: workspace-locked hash specs, deterministic region
//! planning for sampled hashing, BLAKE3 digest computation, and a small
//! reader pool for keeping several reads in flight on a spinning disk.
//!
//! Only live scans ever reach this module -- `tree` JSON imports have no
//! content to read, so they stay permanently unhashed (see `parse.rs`).

use crate::medium;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// The digest-affecting settings for a workspace (see db.rs's `SCHEMA_VERSION`
/// doc and the design spec's §6.1): sample threshold, probe size, and the cap
/// on total probes (head + tail + interior) a sampled hash ever takes.
/// `threshold: None` means "full hash everything" -- implemented as an
/// infinity sentinel rather than a separate boolean, so there is one fewer
/// combination to reason about and one fewer way for two sources to disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashSpec {
    pub threshold: Option<i64>,
    pub probe: i64,
    /// Total probes (including head and tail) a sampled hash ever takes, so
    /// sampled bytes read stays bounded (`max_probes * probe`) no matter how
    /// large the file is -- see `interior_probe_count`'s doc comment for how
    /// this replaces the old fixed-byte-stride approach, which made sampled
    /// bytes grow roughly linearly with file size.
    pub max_probes: i64,
}

impl HashSpec {
    /// A reasonable out-of-the-box default: 32 MiB threshold, 1 MiB probe,
    /// 8 max probes -- used only until a workspace's first hashed scan locks
    /// in whatever spec was actually in force. 32 MiB (rather than a
    /// tighter threshold) is chosen so that files in the 8-32 MiB range get
    /// a full, hash-proven (tier A) comparison instead of a sampled (tier B)
    /// one -- on a typical archive-disc corpus that band is a small slice of
    /// total bytes, so the extra I/O to fully hash it is cheap relative to
    /// the stronger evidence it buys.
    pub fn default_spec() -> Self {
        HashSpec {
            threshold: Some(32 * 1024 * 1024),
            probe: 1024 * 1024,
            max_probes: 8,
        }
    }

    pub fn spec_string(&self) -> String {
        match self.threshold {
            None => "blake3/v1/full".to_string(),
            Some(t) => format!("blake3/v1/th{t}-s{}-m{}", self.probe, self.max_probes),
        }
    }

    /// Hashes are only ever compared within an identical spec string (§6.2)
    /// -- a mismatch must demote a pair to the metadata tiers rather than
    /// comparing, so this needs to round-trip exactly, not fuzzily.
    pub fn parse(s: &str) -> Option<Self> {
        if s == "blake3/v1/full" {
            return Some(HashSpec {
                threshold: None,
                probe: 0,
                max_probes: 0,
            });
        }
        let rest = s.strip_prefix("blake3/v1/th")?;
        let (th, rest) = rest.split_once("-s")?;
        let (probe_str, max_probes_str) = rest.split_once("-m")?;
        Some(HashSpec {
            threshold: Some(th.parse().ok()?),
            probe: probe_str.parse().ok()?,
            max_probes: max_probes_str.parse().ok()?,
        })
    }
}

/// Whether `size` under `spec` would be full-hashed or sampled -- the
/// `hash_kind` recorded on `nodes` and used to distinguish matcher tier A
/// (100, full) from tier B (99, sampled).
fn hash_kind_for(size: i64, spec: &HashSpec) -> &'static str {
    match spec.threshold {
        None => "full",
        Some(t) if size < t => "full",
        Some(_) => "sampled",
    }
}

/// §6.5's region plan: full hash below threshold (or when threshold is
/// infinite), otherwise head + tail (always, at full probe width) plus up to
/// `max_probes - 2` interior probes, merged. Offsets derive **only** from
/// `size` and the spec constants -- never scan order or a running counter --
/// so the same file samples identically on any two media.
#[allow(clippy::single_range_in_vec_init)]
pub fn plan_regions(size: i64, spec: &HashSpec) -> Vec<Range<i64>> {
    let size = size.max(0);
    let full = vec![0..size];
    let Some(threshold) = spec.threshold else {
        return full;
    };
    if size < threshold {
        return full;
    }
    let p = spec.probe.max(0);

    let mut regions = vec![0..p.min(size), (size - p).max(0)..size];
    let n = interior_probe_count(size, threshold, spec.max_probes);
    for i in 1..=n {
        let off = align_down(i * size / (n + 1), 4096);
        regions.push(off..(off + p).min(size));
    }
    merge_overlapping(regions)
}

/// Floor of log2(x), or 0 for non-positive `x`. Pure integer bit-length
/// arithmetic -- no floats -- so it's exactly reproducible on any platform,
/// which matters here because it feeds directly into which bytes get hashed
/// (see `plan_regions`' doc comment on cross-platform determinism).
fn log2_floor(x: i64) -> i64 {
    if x <= 0 {
        0
    } else {
        63 - (x as u64).leading_zeros() as i64
    }
}

/// How many interior probes a sampled hash takes: one more each time `size`
/// doubles past `threshold`, capped at `max_probes - 2` (the interior budget
/// once head and tail are accounted for). This replaces a fixed-byte-stride
/// divisor (`size / stride`), which had no upper bound -- sampled bytes read
/// grew roughly linearly with file size, and a stride large enough to cap
/// that on huge files forced every smaller file down to 0 interior probes.
/// A doubling-based count instead grows smoothly from 0 right at the
/// threshold up to a small fixed cap, so sampled bytes read is bounded by
/// `max_probes * probe` regardless of how large the file is.
fn interior_probe_count(size: i64, threshold: i64, max_probes: i64) -> i64 {
    let max_interior = (max_probes - 2).max(0);
    if max_interior == 0 {
        return 0;
    }
    (log2_floor(size) - log2_floor(threshold.max(1))).clamp(0, max_interior)
}

fn align_down(x: i64, align: i64) -> i64 {
    if align <= 0 {
        return x;
    }
    x - x.rem_euclid(align)
}

fn merge_overlapping(mut regions: Vec<Range<i64>>) -> Vec<Range<i64>> {
    regions.retain(|r| r.start < r.end);
    regions.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<i64>> = Vec::new();
    for r in regions {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                if r.end > last.end {
                    last.end = r.end;
                }
            }
            _ => merged.push(r),
        }
    }
    merged
}

/// Open `path` ahead of a hashing read, hinting the OS toward sequential or
/// random access depending on the region plan (a full hash is one long
/// sequential read; a sampled hash is a handful of scattered probes). The
/// hint is an optimization, never a correctness requirement -- every branch
/// below ignores its own advisory return value, so a failed or unsupported
/// hint must never fail the hash.
#[cfg(windows)]
fn open_for_hashing(path: &Path, sequential: bool) -> std::io::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
    const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;
    let flags = if sequential {
        FILE_FLAG_SEQUENTIAL_SCAN
    } else {
        FILE_FLAG_RANDOM_ACCESS
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // `from_raw_handle` takes ownership, so the handle closes on drop -- no
    // manual `CloseHandle`.
    Ok(unsafe { std::fs::File::from_raw_handle(h as _) })
}

#[cfg(target_os = "linux")]
fn open_for_hashing(path: &Path, sequential: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(path)?;
    let advice = if sequential {
        libc::POSIX_FADV_SEQUENTIAL
    } else {
        libc::POSIX_FADV_RANDOM
    };
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, advice); // 0,0 = whole file
    }
    Ok(f)
}

#[cfg(target_os = "macos")]
fn open_for_hashing(path: &Path, sequential: bool) -> std::io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(path)?;
    unsafe {
        libc::fcntl(
            f.as_raw_fd(),
            libc::F_RDAHEAD,
            if sequential { 1 } else { 0 },
        );
    }
    Ok(f)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open_for_hashing(path: &Path, _sequential: bool) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Hash `path` (a file of `size` bytes) under `spec`, reading `plan_regions`'
/// regions in ascending offset order. Feeds
/// `spec_string || size.to_le_bytes() || region_bytes` into BLAKE3, so a size
/// collision can never become a hash collision and a spec mismatch produces
/// an obviously different digest rather than a subtle one. Returns the
/// digest, the resulting `hash_kind` ("full"/"sampled"), and bytes actually
/// read (for `hash_bytes_read`/coverage accounting).
pub fn hash_file(
    path: &Path,
    size: i64,
    spec: &HashSpec,
) -> std::io::Result<(blake3::Hash, &'static str, i64)> {
    let regions = plan_regions(size, spec);
    let kind = hash_kind_for(size, spec);
    // A single region is one long sequential read (full hash or a
    // below-threshold file); more than one is a handful of scattered probes,
    // where a "sequential" hint would make the OS read ahead across gaps
    // never touched, wasting the very I/O sampling saved.
    let sequential = regions.len() == 1;

    let mut hasher = blake3::Hasher::new();
    hasher.update(spec.spec_string().as_bytes());
    hasher.update(&size.to_le_bytes());

    let mut file = open_for_hashing(path, sequential)?;

    // Sampled reads: hand the kernel every probe's range up front so it can
    // start them all before we block on the first `read_exact` -- the same
    // NCQ-reordering effect the reader pool gets across files, now applied
    // within one large file.
    #[cfg(target_os = "linux")]
    if !sequential {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        for r in &regions {
            unsafe {
                libc::posix_fadvise(fd, r.start, r.end - r.start, libc::POSIX_FADV_WILLNEED);
            }
        }
    }

    let mut bytes_read: i64 = 0;
    for region in &regions {
        let len = (region.end - region.start) as usize;
        if len == 0 {
            continue;
        }
        file.seek(SeekFrom::Start(region.start as u64))?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
        bytes_read += len as i64;
    }
    Ok((hasher.finalize(), kind, bytes_read))
}

/// One hashing candidate: the node to record the result against, its path on
/// disk, and its size (needed for region planning without re-stat'ing).
pub type HashJob = (i64, PathBuf, i64);

/// Worker-pool sizing for `hash_files_pooled`, split by file size into a
/// "small" lane (many concurrent readers -- these are seek-bound anyway on
/// a spinning disk, so a deep queue lets the drive's NCQ reorder them
/// productively) and a "large" lane (usually just one reader -- concurrent
/// multi-GB sequential reads from different physical locations thrash a
/// single HDD head far worse than concurrent small reads do, so the large
/// lane is kept shallow to protect that streaming throughput). Adapted from
/// fclones' own per-device-type thread pools, but computed once up front
/// (never escalated mid-run) since this project can't assume a source is
/// still online later to re-read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneConfig {
    pub small_depth: usize,
    pub large_depth: usize,
    pub large_threshold: i64,
}

impl LaneConfig {
    /// A single flat pool of `queue_depth` workers handling every job,
    /// regardless of size -- the pre-lane-split behavior. Production code
    /// always goes through `from_profile` (SSD's profile already yields this
    /// same shape via `large_file_threshold: i64::MAX`); this constructor
    /// exists only so tests that don't care about the lane split can build a
    /// `LaneConfig` without spelling out all three fields.
    #[cfg(test)]
    pub fn flat(queue_depth: usize) -> Self {
        LaneConfig {
            small_depth: queue_depth,
            large_depth: queue_depth,
            large_threshold: i64::MAX,
        }
    }

    pub fn from_profile(profile: &medium::MediumProfile) -> Self {
        LaneConfig {
            small_depth: profile.reader_queue_depth,
            large_depth: profile.large_lane_queue_depth,
            large_threshold: profile.large_file_threshold,
        }
    }
}

/// Hash `jobs` using two worker pools sized by `lanes` (see `LaneConfig`):
/// jobs under `lanes.large_threshold` run on the small lane, everything else
/// on the large lane, both lanes active concurrently. `LaneConfig::flat`
/// collapses this back to a single pool. Results arrive in completion
/// order, not job order; each is tagged with its node id so the caller (the
/// batched-persistence loop in `commands.rs`) can match them back up.
pub fn hash_files_pooled(
    jobs: Vec<HashJob>,
    spec: HashSpec,
    lanes: LaneConfig,
) -> Vec<(i64, std::io::Result<(blake3::Hash, &'static str, i64)>)> {
    let expected = jobs.len();
    // A stable partition preserves whatever order `resolve_candidates`
    // already sorted `jobs` into (physical-order on HDD) within each lane.
    let (large_jobs, small_jobs): (Vec<HashJob>, Vec<HashJob>) = jobs
        .into_iter()
        .partition(|(_, _, size)| *size >= lanes.large_threshold);

    let (result_tx, result_rx) = crossbeam_channel::unbounded();
    let mut handles = Vec::new();
    for (lane_jobs, depth) in [
        (small_jobs, lanes.small_depth),
        (large_jobs, lanes.large_depth),
    ] {
        if lane_jobs.is_empty() {
            continue;
        }
        // An MPMC channel per lane: every worker clones its own `Receiver`
        // and pulls directly, no `Mutex` serializing access to a single
        // `mpsc::Receiver`. Unbounded is fine -- the full job list is
        // already in memory before this call.
        let (work_tx, work_rx) = crossbeam_channel::unbounded::<HashJob>();
        for job in lane_jobs {
            let _ = work_tx.send(job);
        }
        drop(work_tx);

        let n_workers = depth.max(1);
        for _ in 0..n_workers {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            handles.push(std::thread::spawn(move || {
                while let Ok((node_id, path, size)) = work_rx.recv() {
                    let result = hash_file(&path, size, &spec);
                    if result_tx.send((node_id, result)).is_err() {
                        break;
                    }
                }
            }));
        }
    }
    drop(result_tx);

    let mut results = Vec::with_capacity(expected);
    for r in result_rx {
        results.push(r);
    }
    for h in handles {
        let _ = h.join();
    }
    results
}

// ---------------------------------------------------------------------
// Workspace-level hash-spec locking (§6.1, §6.3)
// ---------------------------------------------------------------------

/// Current hash spec for `workspace_id` (defaulting to `HashSpec::default_spec()`
/// if never set) and whether it is locked. Locking happens automatically the
/// first time a hashing pass actually writes a digest -- see `lock_and_record_spec`.
pub fn get_hash_settings(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<(HashSpec, bool)> {
    let spec_str: Option<String> = conn
        .query_row(
            "SELECT value FROM workspace_state WHERE workspace_id = ?1 AND key = 'hash.spec'",
            params![workspace_id],
            |r| r.get(0),
        )
        .optional()?;
    let locked: bool = conn
        .query_row(
            "SELECT value FROM workspace_state WHERE workspace_id = ?1 AND key = 'hash.locked'",
            params![workspace_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .is_some_and(|v| v == "1");
    let spec = spec_str
        .and_then(|s| HashSpec::parse(&s))
        .unwrap_or_else(HashSpec::default_spec);
    Ok((spec, locked))
}

/// Change the workspace's hash spec. Rejected once locked -- the UI should
/// surface this as "workspace is locked", not retry or silently ignore it.
pub fn set_hash_settings(
    conn: &Connection,
    workspace_id: i64,
    spec: HashSpec,
) -> Result<(), String> {
    let (_, locked) = get_hash_settings(conn, workspace_id).map_err(|e| e.to_string())?;
    if locked {
        return Err(
            "Hash settings are locked for this workspace after its first hashed scan".to_string(),
        );
    }
    conn.execute(
        "INSERT INTO workspace_state (workspace_id, key, value) VALUES (?1, 'hash.spec', ?2)
         ON CONFLICT(workspace_id, key) DO UPDATE SET value = excluded.value",
        params![workspace_id, spec.spec_string()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Record which spec was actually used and lock it in, bypassing the
/// `set_hash_settings` locked-check -- this IS the act of locking, called
/// once (idempotently) after a hashing pass writes its first real digest.
fn lock_and_record_spec(
    tx: &Transaction,
    workspace_id: i64,
    spec: &HashSpec,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO workspace_state (workspace_id, key, value) VALUES (?1, 'hash.spec', ?2)
         ON CONFLICT(workspace_id, key) DO UPDATE SET value = excluded.value",
        params![workspace_id, spec.spec_string()],
    )?;
    tx.execute(
        "INSERT INTO workspace_state (workspace_id, key, value) VALUES (?1, 'hash.locked', '1')
         ON CONFLICT(workspace_id, key) DO UPDATE SET value = excluded.value",
        params![workspace_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------
// Resumable hashing pass
// ---------------------------------------------------------------------

struct CandidateNode {
    node_id: i64,
    rel_path: String,
    size: i64,
    dev: Option<i64>,
    inode: Option<i64>,
    mtime: Option<String>,
}

fn load_hashable_nodes(
    conn: &Connection,
    source_id: i64,
    hash_min_size: i64,
) -> rusqlite::Result<Vec<CandidateNode>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, size, dev, inode, mtime FROM nodes
         WHERE source_id = ?1 AND type = 'file' AND alias_of IS NULL
           AND size >= ?2 AND content_hash IS NULL
         ORDER BY id",
    )?;
    stmt.query_map(params![source_id, hash_min_size], |r| {
        Ok(CandidateNode {
            node_id: r.get(0)?,
            rel_path: r.get(1)?,
            size: r.get(2)?,
            dev: r.get(3)?,
            inode: r.get(4)?,
            mtime: r.get(5)?,
        })
    })?
    .collect()
}

/// The `hash_cache.volume_id` to key on for a node whose live `dev` is
/// `dev`: the source's detected volume id when one was recorded (the NTFS
/// volume serial on Windows -- stable across sessions), falling back to
/// `dev` itself for sources scanned before Stage 5.4 or on platforms where
/// `st_dev` is all that's available. A stale `st_dev` on its own just causes
/// a cache miss (harmless); the volume id exists to rule out the case where
/// a *reused* `st_dev` also happens to collide on `(file_id, size, mtime)`
/// and returns a wrong digest -- low probability, unbounded consequence,
/// and the one subsystem here where a wrong answer is unrecoverable.
fn cache_volume_id(source_volume_id: &Option<String>, dev: i64) -> String {
    match source_volume_id {
        Some(v) if !v.is_empty() => v.clone(),
        _ => dev.to_string(),
    }
}

/// Apply any already-cached digest to matching nodes with zero file reads
/// (a re-scan of an unchanged tree should touch no content at all), then
/// return the remaining nodes that still need real hashing -- in physical
/// order on HDD, insertion order otherwise.
fn resolve_candidates(
    tx: &Transaction,
    source_id: i64,
    source_volume_id: &Option<String>,
    hash_min_size: i64,
    spec: &HashSpec,
    sort_by_physical_order: bool,
) -> rusqlite::Result<(Vec<CandidateNode>, usize)> {
    let spec_str = spec.spec_string();
    let all = load_hashable_nodes(tx, source_id, hash_min_size)?;

    let mut remaining = Vec::new();
    let mut cached = 0usize;
    // The real bytes a cache hit covers -- `node.size`, not
    // `hash_bytes_read` (a read-cost metric that isn't even recoverable from
    // a `hash_cache` row). Without this, a re-scan of an unchanged tree
    // reports near-zero coverage despite every file already being hashed.
    let mut cached_bytes = 0i64;
    for node in all {
        let hit: Option<(Vec<u8>, String, String)> = match (node.dev, node.inode) {
            (Some(dev), Some(inode)) => tx
                .query_row(
                    "SELECT content_hash, hash_kind, computed_at FROM hash_cache
                     WHERE volume_id = ?1 AND file_id = ?2 AND size = ?3 AND mtime = ?4 AND hash_spec = ?5",
                    params![
                        cache_volume_id(source_volume_id, dev),
                        inode,
                        node.size,
                        node.mtime.clone().unwrap_or_default(),
                        spec_str
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?,
            _ => None,
        };
        match hit {
            Some((content_hash, hash_kind, computed_at)) => {
                tx.execute(
                    "UPDATE nodes SET content_hash = ?1, hash_kind = ?2, hash_spec = ?3, hashed_at = ?4 WHERE id = ?5",
                    params![content_hash, hash_kind, spec_str, computed_at, node.node_id],
                )?;
                cached += 1;
                cached_bytes += node.size;
            }
            None => remaining.push(node),
        }
    }
    if cached > 0 {
        tx.execute(
            "UPDATE sources SET hash_coverage_files = hash_coverage_files + ?1, hash_coverage_bytes = hash_coverage_bytes + ?2 WHERE id = ?3",
            params![cached as i64, cached_bytes, source_id],
        )?;
    }
    if sort_by_physical_order {
        remaining.sort_by_key(|n| (n.dev.is_none(), n.dev, n.inode, n.node_id));
    }
    Ok((remaining, cached))
}

/// Outcome of one `run_hash_scan` call (which may itself be a resume of a
/// previously-interrupted pass).
#[derive(Debug, Default)]
pub struct HashScanReport {
    pub hashed: usize,
    pub cached: usize,
    pub errors: Vec<(i64, String)>,
    /// True if this call stopped early because its cancel flag was set,
    /// rather than because every candidate was processed.
    pub cancelled: bool,
}

/// Per-source cooperative cancellation flags for an in-progress
/// `run_hash_scan` call, managed as Tauri state. A source only has an entry
/// while a hash pass for it is actually running (`commands.rs::run_hash_scan`
/// inserts one before calling `run_hash_scan` and removes it after), so
/// `cancel_hash_scan` finding no entry for a source just means nothing is
/// currently hashing it -- a harmless no-op, not an error.
#[derive(Default)]
pub struct HashCancelFlags(pub Mutex<HashMap<i64, Arc<AtomicBool>>>);

const HASH_BATCH_SIZE: usize = 2000;

/// Hash every eligible file in `source_id` under the workspace's (locked or
/// default) hash spec that doesn't already have a digest, in batches of
/// `HASH_BATCH_SIZE` committed one at a time. Follows `run_dedup`'s pattern
/// of taking its own connection rather than holding the shared `Db` mutex
/// for a potentially multi-hour pass -- see `commands.rs`'s doc comment.
///
/// Resumability comes entirely from `content_hash IS NULL` being the
/// candidate filter: since each batch's `nodes` update and `scan_progress`
/// update commit in the same transaction, a candidate is never "in-flight"
/// across a crash -- it's either fully hashed or not attempted yet. Calling
/// this again after any interruption (or never having started) always does
/// the right thing by construction, with no separate resume path to keep in
/// sync. `scan_progress.last_cursor` is therefore purely informational (the
/// cumulative hashed-file count for this source, continuing across calls
/// rather than restarting at zero) -- **not** used to slice the candidate
/// list. An index-based cursor would desync from this filter: on a resumed
/// call the candidate list is already shorter (everything hashed so far is
/// filtered out), so slicing it by an old absolute offset would silently
/// skip unprocessed files.
///
/// A `tree` JSON import has no `orig_root_path` to read content from and is
/// always a no-op here.
pub fn run_hash_scan(
    conn: &mut Connection,
    source_id: i64,
    lanes: LaneConfig,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64),
) -> rusqlite::Result<HashScanReport> {
    let (workspace_id, orig_root_path, hash_min_size, medium_kind_str, source_volume_id): (
        i64,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT workspace_id, orig_root_path, hash_min_size, medium_kind, volume_id FROM sources WHERE id = ?1",
        params![source_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    let Some(root) = orig_root_path else {
        return Ok(HashScanReport::default());
    };
    let (spec, _locked) = get_hash_settings(conn, workspace_id)?;
    let medium_kind = medium_kind_str
        .as_deref()
        .and_then(medium::MediumKind::parse)
        .unwrap_or(medium::MediumKind::Unknown);
    let sort_by_physical_order = medium::profile_for(medium_kind).sort_by_physical_order;

    let tx = conn.transaction()?;
    let (candidates, cached) = resolve_candidates(
        &tx,
        source_id,
        &source_volume_id,
        hash_min_size,
        &spec,
        sort_by_physical_order,
    )?;
    tx.commit()?;

    // Baseline for the informational cursor: files already hashed for this
    // source before this call, so a resumed run's reported progress
    // continues rather than restarting from zero.
    let already_hashed: i64 = conn.query_row(
        "SELECT COUNT(*) FROM nodes
         WHERE source_id = ?1 AND type = 'file' AND alias_of IS NULL
           AND size >= ?2 AND content_hash IS NOT NULL",
        params![source_id, hash_min_size],
        |r| r.get(0),
    )?;

    let mut cursor = 0usize;
    let total = candidates.len() as u64;
    let mut hashed = 0usize;
    let mut errors = Vec::new();
    let mut cancelled = false;

    while cursor < candidates.len() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let end = (cursor + HASH_BATCH_SIZE).min(candidates.len());
        let batch = &candidates[cursor..end];
        let jobs: Vec<HashJob> = batch
            .iter()
            .map(|c| (c.node_id, Path::new(&root).join(&c.rel_path), c.size))
            .collect();
        let results = hash_files_pooled(jobs, spec, lanes);

        let ts = now();
        let tx = conn.transaction()?;
        let mut batch_bytes = 0i64;
        let mut batch_files = 0i64;
        for (node_id, result) in results {
            let Some(node) = batch.iter().find(|c| c.node_id == node_id) else {
                continue;
            };
            match result {
                Ok((digest, kind, bytes_read)) => {
                    let digest_bytes = digest.as_bytes().to_vec();
                    tx.execute(
                        "UPDATE nodes SET content_hash = ?1, hash_kind = ?2, hash_spec = ?3, hash_bytes_read = ?4, hashed_at = ?5 WHERE id = ?6",
                        params![digest_bytes, kind, spec.spec_string(), bytes_read, ts, node_id],
                    )?;
                    if let (Some(dev), Some(inode)) = (node.dev, node.inode) {
                        tx.execute(
                            "INSERT INTO hash_cache (volume_id, file_id, size, mtime, hash_spec, content_hash, hash_kind, computed_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                             ON CONFLICT(volume_id, file_id, size, mtime, hash_spec)
                             DO UPDATE SET content_hash = excluded.content_hash, hash_kind = excluded.hash_kind, computed_at = excluded.computed_at",
                            params![
                                cache_volume_id(&source_volume_id, dev),
                                inode,
                                node.size,
                                node.mtime.clone().unwrap_or_default(),
                                spec.spec_string(),
                                digest_bytes,
                                kind,
                                ts
                            ],
                        )?;
                    }
                    hashed += 1;
                    batch_files += 1;
                    batch_bytes += bytes_read;
                }
                Err(e) => errors.push((node_id, e.to_string())),
            }
        }
        tx.execute(
            "UPDATE sources SET hash_spec = ?1, hash_coverage_files = hash_coverage_files + ?2, hash_coverage_bytes = hash_coverage_bytes + ?3 WHERE id = ?4",
            params![spec.spec_string(), batch_files, batch_bytes, source_id],
        )?;
        tx.execute(
            "INSERT INTO scan_progress (source_id, phase, last_cursor, updated_at) VALUES (?1, 'hashing', ?2, ?3)
             ON CONFLICT(source_id) DO UPDATE SET phase = 'hashing', last_cursor = excluded.last_cursor, updated_at = excluded.updated_at",
            params![source_id, already_hashed + hashed as i64, ts],
        )?;
        tx.commit()?;

        cursor = end;
        on_progress(
            already_hashed as u64 + hashed as u64,
            already_hashed as u64 + total,
        );
    }

    let tx = conn.transaction()?;
    if !cancelled {
        tx.execute(
            "INSERT INTO scan_progress (source_id, phase, last_cursor, updated_at) VALUES (?1, 'done', ?2, ?3)
             ON CONFLICT(source_id) DO UPDATE SET phase = 'done', last_cursor = excluded.last_cursor, updated_at = excluded.updated_at",
            params![source_id, already_hashed + hashed as i64, now()],
        )?;
    }
    if hashed > 0 || cached > 0 {
        lock_and_record_spec(&tx, workspace_id, &spec)?;
    }
    tx.commit()?;

    Ok(HashScanReport {
        hashed,
        cached,
        errors,
        cancelled,
    })
}

/// Current `scan_progress` phase/cursor for `source_id`, if any -- drives a
/// "Resume hashing" banner when `phase == "hashing"` (interrupted) rather
/// than `"done"` or absent (never started).
pub fn get_scan_progress(
    conn: &Connection,
    source_id: i64,
) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(
        "SELECT phase, last_cursor FROM scan_progress WHERE source_id = ?1",
        params![source_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// `scan_progress.phase` for every source in `workspace_id` that has one --
/// `source_list` merges this in so the source-list UI can offer a "Resume
/// hashing" affordance for any source whose phase is `"hashing"` (paused or
/// interrupted) without a per-source round trip. Mirrors
/// `rollup::duplicated_size_by_source`'s map-then-merge shape.
pub fn hash_phase_by_source(
    conn: &Connection,
    workspace_id: i64,
) -> rusqlite::Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare(
        "SELECT sp.source_id, sp.phase FROM scan_progress sp
         JOIN sources s ON s.id = sp.source_id
         WHERE s.workspace_id = ?1",
    )?;
    let rows = stmt.query_map(params![workspace_id], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "adedup_{label}_{}_{}_{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// There's no portable way to assert an fadvise/CreateFileW-flag hint
    /// actually took effect -- what's testable, and what actually matters
    /// for correctness, is that `open_for_hashing` still hands back a normal
    /// readable file regardless of the `sequential` hint.
    #[test]
    fn open_for_hashing_reads_identical_bytes_to_plain_open() {
        let dir = unique_temp_dir("open_for_hashing");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        let content: Vec<u8> = (0..10_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &content).unwrap();

        for sequential in [true, false] {
            let mut f = open_for_hashing(&path, sequential).unwrap();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).unwrap();
            assert_eq!(buf, content, "sequential={sequential}");
        }

        let (digest, _, _) =
            hash_file(&path, content.len() as i64, &HashSpec::default_spec()).unwrap();
        let expected = blake3::Hasher::new()
            .update(HashSpec::default_spec().spec_string().as_bytes())
            .update(&(content.len() as i64).to_le_bytes())
            .update(&content)
            .finalize();
        assert_eq!(digest, expected);
    }

    /// Build an in-memory workspace with one real `kind='scan'` source over
    /// a temp directory containing `files`, so `run_hash_scan` can actually
    /// open and read them. Returns (conn, workspace_id, source_id, temp_dir).
    fn setup_scan_source(files: &[(&str, &[u8])]) -> (Connection, i64, i64, std::path::PathBuf) {
        let dir = unique_temp_dir("hashscan");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
        }

        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        // volume_id must parse as an i64: on the Windows fast scan path
        // (scan.rs::scan_folder_fast) it's the only source of `nodes.dev`,
        // so an empty string here would leave every node's `dev` NULL and
        // break any test relying on a real (dev, inode) pair -- Unix's
        // `inode_dev` ignores this field and always pulls a real `st_dev`
        // from `stat()`, which is why this only ever showed up on Windows.
        let no_medium = crate::medium::MediumInfo {
            medium_kind: crate::medium::MediumKind::Unknown,
            filesystem: None,
            volume_id: "1".to_string(),
        };
        let flat = crate::scan::scan_folder(&dir, &no_medium, |_| {}).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, imported_at, total_size, file_count, hash_min_size)
             VALUES (?1, 'scan', 'disc-a', 'disc-a', ?2, 't', ?3, ?4, 0)",
            params![
                ws,
                dir.to_string_lossy().to_string(),
                flat.total_size,
                flat.file_count
            ],
        )
        .unwrap();
        let source_id = tx.last_insert_rowid();
        crate::parse::insert_nodes(&tx, source_id, &flat).unwrap();
        tx.commit().unwrap();

        (conn, ws, source_id, dir)
    }

    fn sampled_spec() -> HashSpec {
        HashSpec {
            threshold: Some(8 * 1024 * 1024),
            probe: 1024 * 1024,
            max_probes: 8,
        }
    }

    #[test]
    fn spec_string_round_trips_for_full_and_sampled() {
        let full = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        };
        assert_eq!(HashSpec::parse(&full.spec_string()), Some(full));

        let sampled = sampled_spec();
        assert_eq!(HashSpec::parse(&sampled.spec_string()), Some(sampled));
        assert_eq!(sampled.spec_string(), "blake3/v1/th8388608-s1048576-m8");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(HashSpec::parse("not-a-spec"), None);
        assert_eq!(HashSpec::parse("blake3/v1/thX-s1-m1"), None);
    }

    #[test]
    fn small_file_is_fully_hashed_as_one_region() {
        let spec = sampled_spec();
        let regions = plan_regions(1000, &spec);
        assert_eq!(regions, vec![0..1000]);
    }

    #[test]
    fn infinite_threshold_always_fully_hashes() {
        let spec = HashSpec {
            threshold: None,
            probe: 1024,
            max_probes: 4096,
        };
        let regions = plan_regions(50_000_000, &spec);
        assert_eq!(regions, vec![0..50_000_000]);
    }

    #[test]
    fn large_file_head_and_tail_are_always_present() {
        let spec = sampled_spec();
        let size = 100 * 1024 * 1024; // well above threshold, n=3 interior probes
        let regions = plan_regions(size, &spec);
        assert_eq!(regions.first().unwrap().start, 0, "head must start at 0");
        assert_eq!(
            regions.last().unwrap().end,
            size,
            "tail must reach the end of the file"
        );
        assert!(regions.len() >= 2);
    }

    #[test]
    fn region_plan_is_deterministic_across_repeated_calls() {
        let spec = sampled_spec();
        let size = 40 * 1024 * 1024;
        assert_eq!(plan_regions(size, &spec), plan_regions(size, &spec));
    }

    #[test]
    fn interior_probe_count_is_flat_zero_up_to_the_first_doubling_past_threshold() {
        let spec = sampled_spec();
        // Anywhere in [threshold, 2*threshold) => 0 interior probes: only
        // head+tail (2 total, possibly merged into 1 region if they overlap).
        for size in [spec.threshold.unwrap(), spec.threshold.unwrap() * 2 - 1] {
            let regions = plan_regions(size, &spec);
            assert!(
                regions.len() <= 2,
                "size {size} should have no interior probes yet, got {regions:?}"
            );
        }
    }

    #[test]
    fn interior_probe_count_increases_by_one_at_each_size_doubling() {
        let spec = sampled_spec();
        let threshold = spec.threshold.unwrap();
        let max_interior = spec.max_probes - 2;

        let mut prev_len = 0usize;
        for k in 0..=(max_interior + 2) {
            let size = threshold * (1i64 << k);
            let regions = plan_regions(size, &spec);
            assert!(
                regions.len() >= prev_len,
                "region count must never decrease as size grows: k={k}, size={size}"
            );
            prev_len = regions.len();
        }
        // By the time size has doubled past threshold enough times, the cap
        // (max_probes total) must have been reached.
        let capped = plan_regions(threshold * (1i64 << (max_interior + 2)), &spec);
        assert_eq!(capped.len() as i64, spec.max_probes);
    }

    /// An exact-count assertion at a deliberately non-saturated size, so a
    /// wrong clamp bound (e.g. off-by-one on `max_interior`) or a formula
    /// that jumps/stalls between doublings would fail this even though the
    /// saturation tests above (which only exercise already-capped sizes)
    /// would not catch it.
    #[test]
    fn interior_probe_count_is_exactly_two_at_the_second_doubling_past_threshold() {
        let spec = sampled_spec(); // threshold = 8 MiB, max_probes = 8
        // log2_floor(32 MiB) = 25, log2_floor(8 MiB) = 23 => n = 2 interior
        // probes, plus head and tail => 4 regions total.
        let regions = plan_regions(32 * 1024 * 1024, &spec);
        assert_eq!(regions.len(), 4);
    }

    #[test]
    fn size_smaller_than_probe_merges_head_and_tail_into_one_region() {
        // A spec where the probe is wider than the threshold (unusual, but
        // not disallowed) -- head (0..size) and tail (0..size) fully overlap
        // whenever size <= probe, regardless of interior probe count.
        let spec = HashSpec {
            threshold: Some(100),
            probe: 1000,
            max_probes: 8,
        };
        let size = 150; // just above threshold, well below probe
        let regions = plan_regions(size, &spec);
        assert_eq!(regions, vec![0..size]);
    }

    #[test]
    fn identical_content_hashes_identically_full_and_sampled() {
        let dir = std::env::temp_dir().join(format!("adedup_hash_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let content = b"the quick brown fox jumps over the lazy dog".repeat(100);

        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, &content).unwrap();
        std::fs::write(&b, &content).unwrap();

        let spec = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        }; // full hash, simplest to reason about
        let (hash_a, kind_a, read_a) = hash_file(&a, content.len() as i64, &spec).unwrap();
        let (hash_b, kind_b, read_b) = hash_file(&b, content.len() as i64, &spec).unwrap();
        assert_eq!(hash_a, hash_b);
        assert_eq!(kind_a, "full");
        assert_eq!(kind_a, kind_b);
        assert_eq!(read_a, content.len() as i64);
        assert_eq!(read_a, read_b);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn different_content_hashes_differently() {
        let dir = std::env::temp_dir().join(format!("adedup_hash_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, b"hello world").unwrap();
        std::fs::write(&b, b"hello worlD").unwrap();

        let spec = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        };
        let (hash_a, ..) = hash_file(&a, 11, &spec).unwrap();
        let (hash_b, ..) = hash_file(&b, 11, &spec).unwrap();
        assert_ne!(hash_a, hash_b);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Guards against a regression that would make interior probes silent
    /// no-ops (e.g. an off-by-one in `plan_regions` that never actually
    /// reads the differing byte). Two same-size files with identical head
    /// and tail bytes, differing only at an offset deterministically inside
    /// one of `plan_regions`' interior probes, must still hash differently
    /// -- this is the sampled-tier detection this project's low-false-
    /// positive design relies on.
    #[test]
    fn sampled_hash_detects_a_difference_that_falls_inside_a_probed_region() {
        let dir =
            std::env::temp_dir().join(format!("adedup_hash_test_probe_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let spec = HashSpec {
            threshold: Some(8192),
            probe: 64,
            max_probes: 8,
        };
        let size: usize = 20_000;
        // log2_floor(20000) = 14 (2^14 = 16384 <= 20000 < 32768 = 2^15),
        // log2_floor(8192) = 13, so n = clamp(14-13, 0, 6) = 1 interior probe
        // at align_down(1*20000/2, 4096) = align_down(10000, 4096) = 8192,
        // covering [8192..8256). Head is [0..64), tail is [19936..20000).
        // Offset 8200 falls inside that interior probe and outside both
        // head and tail, so only a real interior read detects a difference
        // there.
        assert!(regions_cover_offset(
            &plan_regions(size as i64, &spec),
            8200
        ));
        assert!(!(0..64).contains(&8200) && !(19936..20_000).contains(&8200));

        let mut content_a = vec![0u8; size];
        for (i, b) in content_a.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut content_b = content_a.clone();
        content_b[8200] ^= 0xFF;

        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        std::fs::write(&a, &content_a).unwrap();
        std::fs::write(&b, &content_b).unwrap();

        let (hash_a, kind_a, _) = hash_file(&a, size as i64, &spec).unwrap();
        let (hash_b, kind_b, _) = hash_file(&b, size as i64, &spec).unwrap();
        assert_eq!(kind_a, "sampled");
        assert_eq!(kind_b, "sampled");
        assert_ne!(
            hash_a, hash_b,
            "a difference inside an interior probe region must change the digest"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    fn regions_cover_offset(regions: &[Range<i64>], offset: i64) -> bool {
        regions.iter().any(|r| r.contains(&offset))
    }

    /// Pins the deliberately-chosen default threshold so any future change
    /// to it is an explicit, visible edit rather than an accidental drift.
    #[test]
    fn default_spec_threshold_matches_the_32mib_corpus_cutoff() {
        assert_eq!(HashSpec::default_spec().threshold, Some(32 * 1024 * 1024));
    }

    /// Pins the deliberately-chosen default probe cap so any future change
    /// to it is an explicit, visible edit rather than an accidental drift.
    #[test]
    fn default_spec_max_probes_is_eight() {
        assert_eq!(HashSpec::default_spec().max_probes, 8);
    }

    #[test]
    fn interior_probe_count_saturates_for_arbitrarily_large_files() {
        let spec = HashSpec {
            threshold: Some(1024),
            probe: 16,
            max_probes: 8,
        };
        // However large the file, total regions never exceed max_probes --
        // this is the core bounded-sampled-bytes guarantee this design
        // exists for (sampled bytes read is capped at max_probes * probe,
        // never growing with file size once past a handful of doublings).
        // 100 GiB is already far past saturation for this spec and stays
        // safely within i64 arithmetic used internally by `plan_regions`.
        let regions = plan_regions(100 * 1024 * 1024 * 1024, &spec);
        assert_eq!(regions.len() as i64, spec.max_probes);
    }

    #[test]
    fn probes_spread_across_the_full_file_not_clustered_near_the_start() {
        let spec = HashSpec {
            threshold: Some(1024),
            probe: 16,
            max_probes: 8,
        };
        let size = 10 * 1024 * 1024 * 1024; // large enough to hit the cap
        let regions = plan_regions(size, &spec);
        assert_eq!(regions.len() as i64, spec.max_probes);
        // The last interior probe (second-to-last region; the very last is
        // the tail) must land well past the start of the file, proving the
        // proportional placement formula spreads probes across the whole
        // file rather than bunching them in the first few probe-widths.
        let last_interior = &regions[regions.len() - 2];
        assert!(
            last_interior.start > size * 3 / 4,
            "last interior probe at {last_interior:?} should be past 3/4 of size {size}"
        );
    }

    #[test]
    fn max_probes_of_two_forces_head_and_tail_only_regardless_of_size() {
        let spec = HashSpec {
            threshold: Some(1024),
            probe: 16,
            max_probes: 2,
        };
        for size in [2048, 1024 * 1024, 10 * 1024 * 1024 * 1024] {
            let regions = plan_regions(size, &spec);
            assert!(
                regions.len() <= 2,
                "max_probes=2 must never add interior probes (size={size}, got {regions:?})"
            );
        }
    }

    #[test]
    fn differing_spec_produces_a_different_digest_for_identical_bytes() {
        let dir = std::env::temp_dir().join(format!("adedup_hash_test3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.bin");
        let content: &[u8] = b"same bytes, different spec";
        std::fs::write(&path, content).unwrap();
        let size = content.len() as i64;

        let spec_a = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        };
        let spec_b = HashSpec {
            threshold: Some(1),
            probe: 4096,
            max_probes: 4096,
        };
        let (hash_a, ..) = hash_file(&path, size, &spec_a).unwrap();
        let (hash_b, ..) = hash_file(&path, size, &spec_b).unwrap();
        assert_ne!(
            hash_a, hash_b,
            "the spec string is part of the hash input, so a spec change must change the digest"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_files_pooled_processes_every_job_and_tags_results_correctly() {
        let dir = std::env::temp_dir().join(format!("adedup_pool_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut jobs = Vec::new();
        for i in 0..20 {
            let path = dir.join(format!("f{i}.bin"));
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "content-{i}").unwrap();
            let size = format!("content-{i}").len() as i64;
            jobs.push((i as i64, path, size));
        }

        let spec = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        };
        let results = hash_files_pooled(jobs, spec, LaneConfig::flat(4));
        assert_eq!(results.len(), 20);

        let mut seen: Vec<i64> = results.iter().map(|(id, _)| *id).collect();
        seen.sort();
        assert_eq!(seen, (0..20).collect::<Vec<_>>());
        for (_, r) in &results {
            assert!(r.is_ok());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_files_pooled_splits_jobs_across_both_lanes_and_processes_every_job() {
        let dir = std::env::temp_dir().join(format!("adedup_lane_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut jobs = Vec::new();
        // Half the jobs are "small" (below the 50-byte lane threshold), half
        // "large" -- exercising both lanes in one call.
        for i in 0..20 {
            let content = if i < 10 {
                format!("s{i}")
            } else {
                format!("large-content-padded-out-{i:03}")
            };
            let path = dir.join(format!("f{i}.bin"));
            let mut f = std::fs::File::create(&path).unwrap();
            write!(f, "{content}").unwrap();
            jobs.push((i as i64, path, content.len() as i64));
        }

        let spec = HashSpec {
            threshold: None,
            probe: 0,
            max_probes: 0,
        };
        let lanes = LaneConfig {
            small_depth: 3,
            large_depth: 1,
            large_threshold: 20,
        };
        let results = hash_files_pooled(jobs, spec, lanes);
        assert_eq!(results.len(), 20);

        let mut seen: Vec<i64> = results.iter().map(|(id, _)| *id).collect();
        seen.sort();
        assert_eq!(seen, (0..20).collect::<Vec<_>>());
        for (_, r) in &results {
            assert!(r.is_ok());
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hash_settings_default_to_a_sane_spec_and_start_unlocked() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        let (spec, locked) = get_hash_settings(&conn, ws).unwrap();
        assert_eq!(spec, HashSpec::default_spec());
        assert!(!locked);
    }

    #[test]
    fn hash_settings_set_then_get_round_trips_before_locking() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();

        let custom = HashSpec {
            threshold: Some(1024),
            probe: 512,
            max_probes: 4096,
        };
        set_hash_settings(&conn, ws, custom).unwrap();
        let (spec, locked) = get_hash_settings(&conn, ws).unwrap();
        assert_eq!(spec, custom);
        assert!(!locked);
    }

    #[test]
    fn run_hash_scan_hashes_files_and_locks_the_workspace() {
        let (mut conn, ws, source_id, dir) =
            setup_scan_source(&[("a.bin", b"hello world"), ("b.bin", b"a different file")]);

        let (_, locked_before) = get_hash_settings(&conn, ws).unwrap();
        assert!(!locked_before);

        let report = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.hashed, 2);
        assert_eq!(report.cached, 0);
        assert!(report.errors.is_empty());

        let (_, locked_after) = get_hash_settings(&conn, ws).unwrap();
        assert!(
            locked_after,
            "the workspace must lock its hash spec after the first hashed write"
        );

        // set_hash_settings must now be rejected.
        assert!(set_hash_settings(&conn, ws, HashSpec::default_spec()).is_err());

        // Every hashed node actually got a digest, and scan_progress is 'done'.
        let hashed_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE source_id = ?1 AND content_hash IS NOT NULL",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hashed_count, 2);
        let phase: String = conn
            .query_row(
                "SELECT phase FROM scan_progress WHERE source_id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "done");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_hash_scan_is_a_no_op_for_json_sources() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'json', 'disc-a', 'disc-a', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();

        let report = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.hashed, 0);
        assert_eq!(report.cached, 0);
    }

    #[test]
    fn cache_hit_skips_content_read_entirely() {
        let (mut conn, ws, source_id, dir) = setup_scan_source(&[("a.bin", b"hello world")]);
        let spec = get_hash_settings(&conn, ws).unwrap().0;

        let (dev, inode, size, mtime): (i64, i64, i64, Option<String>) = conn
            .query_row(
                "SELECT dev, inode, size, mtime FROM nodes WHERE source_id = ?1 AND name = 'a.bin'",
                params![source_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO hash_cache (volume_id, file_id, size, mtime, hash_spec, content_hash, hash_kind, computed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'full', 't')",
            params![dev.to_string(), inode, size, mtime.unwrap_or_default(), spec.spec_string(), vec![0xAB_u8; 32]],
        )
        .unwrap();

        // Delete the real file on disk -- if the cache hit didn't work, a
        // real hash attempt would fail with a read error, not just produce
        // the wrong digest, making this a strong test of "zero content reads".
        std::fs::remove_file(dir.join("a.bin")).unwrap();

        let report = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.hashed, 0, "no real hashing should have happened");
        assert_eq!(report.cached, 1);
        assert!(report.errors.is_empty());

        let content_hash: Vec<u8> = conn
            .query_row(
                "SELECT content_hash FROM nodes WHERE source_id = ?1 AND name = 'a.bin'",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(content_hash, vec![0xAB_u8; 32]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_second_run_after_completion_hashes_nothing_more() {
        let files: Vec<(&str, &[u8])> =
            vec![("a.bin", b"aaa"), ("b.bin", b"bbb"), ("c.bin", b"ccc")];
        let (mut conn, _ws, source_id, dir) = setup_scan_source(&files);

        let first = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(first.hashed, 3);

        let second = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            second.hashed, 0,
            "nothing is left to hash once every candidate has a digest"
        );
        assert!(second.errors.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_run_after_a_simulated_interruption_only_hashes_what_never_committed() {
        // A crash mid-batch never leaves a *partial* hash (the nodes update
        // and scan_progress update commit in the same transaction) -- the
        // realistic post-crash state is "some files have a full commit, the
        // rest simply never got their content_hash set at all". Simulate
        // that directly by clearing one file's hash back to NULL after a
        // full run, rather than by fabricating a cursor value.
        let files: Vec<(&str, &[u8])> =
            vec![("a.bin", b"aaa"), ("b.bin", b"bbb"), ("c.bin", b"ccc")];
        let (mut conn, _ws, source_id, dir) = setup_scan_source(&files);

        let first = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(first.hashed, 3);

        let b_id: i64 = conn
            .query_row(
                "SELECT id FROM nodes WHERE source_id = ?1 AND name = 'a.bin'",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        // A sentinel value on the OTHER two files' hash_spec proves they are
        // never re-touched by the resumed call below.
        conn.execute(
            "UPDATE nodes SET hash_spec = 'sentinel' WHERE source_id = ?1 AND name != 'a.bin'",
            params![source_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE nodes SET content_hash = NULL, hash_kind = NULL, hash_spec = NULL, hashed_at = NULL WHERE id = ?1",
            params![b_id],
        )
        .unwrap();
        // A real crash never commits the node update *or* the hash_cache
        // write it shares a transaction with -- clear both, or this "resume"
        // would just be served from cache instead of exercising a real
        // re-hash (already covered separately by `cache_hit_skips_content_read_entirely`).
        conn.execute("DELETE FROM hash_cache", []).unwrap();

        let resumed = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            resumed.hashed, 1,
            "only the one file with a cleared digest should be re-hashed"
        );

        let untouched_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE source_id = ?1 AND name != 'a.bin' AND hash_spec = 'sentinel'",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            untouched_count, 2,
            "already-hashed files must not be re-processed by the resumed call"
        );
        let new_content_hash: Option<Vec<u8>> = conn
            .query_row(
                "SELECT content_hash FROM nodes WHERE id = ?1",
                params![b_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            new_content_hash.is_some(),
            "the cleared file must have a real digest again after the resumed call"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancelling_mid_run_stops_before_finishing_and_stays_resumable() {
        // Enough files to span two HASH_BATCH_SIZE-sized batches, so the
        // cancel flag (flipped from inside the first batch's on_progress
        // callback, simulating a user clicking Cancel while it's running)
        // has a second loop iteration in which to actually be observed.
        let names: Vec<String> = (0..(HASH_BATCH_SIZE + 5))
            .map(|i| format!("f{i:05}.bin"))
            .collect();
        let contents: Vec<[u8; 4]> = (0..(HASH_BATCH_SIZE + 5))
            .map(|i| (i as u32).to_le_bytes())
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .zip(contents.iter())
            .map(|(n, c)| (n.as_str(), c.as_slice()))
            .collect();
        let (mut conn, _ws, source_id, dir) = setup_scan_source(&files);

        let cancel = AtomicBool::new(false);
        let mut batches_seen = 0u32;
        let first = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &cancel,
            |_current, _total| {
                batches_seen += 1;
                if batches_seen == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap();

        assert!(
            first.cancelled,
            "the run must report cancellation once the flag was observed"
        );
        assert_eq!(
            first.hashed, HASH_BATCH_SIZE,
            "only the already-committed first batch should be hashed before stopping"
        );

        let phase: String = conn
            .query_row(
                "SELECT phase FROM scan_progress WHERE source_id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            phase, "hashing",
            "a cancelled run must leave scan_progress at 'hashing', never 'done'"
        );

        let resumed = run_hash_scan(
            &mut conn,
            source_id,
            LaneConfig::flat(4),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            resumed.hashed, 5,
            "resuming must hash exactly what the cancelled run left behind"
        );
        assert!(!resumed.cancelled);

        let phase_after: String = conn
            .query_row(
                "SELECT phase FROM scan_progress WHERE source_id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase_after, "done");

        std::fs::remove_dir_all(&dir).ok();
    }
}
