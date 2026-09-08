//! Live folder scanning as a `tree`-independent alternative. Produces the same
//! `Flattened` structure as `parse.rs` so both import paths share insertion.

use crate::medium;
use crate::parse::{FlatNode, Flattened};
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;
use walkdir::WalkDir;

/// Format a filesystem timestamp to match the `tree -D` output format.
fn format_time(t: SystemTime) -> Option<String> {
    let dt: DateTime<Local> = t.into();
    Some(dt.format("%Y-%m-%d_%H:%M:%S").to_string())
}

/// Extract the inode and device id in a cross-platform way. Only used by
/// `scan_folder_walkdir` -- the Windows fast path (`scan_folder_fast`)
/// resolves file id and volume identity straight from
/// `GetFileInformationByHandleEx`'s enumeration, without a per-file open.
#[cfg(unix)]
fn inode_dev(_path: &Path, meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    use std::os::unix::fs::MetadataExt;
    (Some(meta.ino() as i64), Some(meta.dev() as i64))
}

/// On Windows, `Metadata::file_index`/`volume_serial_number` are still
/// gated behind the unstable `windows_by_handle` feature on stable Rust,
/// so query the same info manually via `GetFileInformationByHandle`. Only
/// reached by the `scan_folder_walkdir` fallback path -- see the module doc
/// on `scan_folder_fast` for why the primary Windows path avoids this
/// per-file open entirely.
#[cfg(windows)]
fn inode_dev(path: &Path, _meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };

    let handle = file.as_raw_handle();
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle as _, &mut info) };
    if ok == 0 {
        return (None, None);
    }

    let file_index = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    (
        Some(file_index as i64),
        Some(info.dwVolumeSerialNumber as i64),
    )
}

#[cfg(not(any(unix, windows)))]
fn inode_dev(_path: &Path, _meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    (None, None)
}

/// Recursively scan `root`, returning a flattened node list rooted at the
/// entries directly under `root` (matching `parse_tree_json` semantics).
///
/// `medium_info` (already detected by the caller before the scan starts) is
/// used on Windows to pick the fast directory-handle enumeration path over
/// `FileIdExtd*` (ReFS, 128-bit ids) vs `FileIdBoth*` (everything else), and
/// to supply the volume serial as every node's `dev` -- resolved once here
/// rather than once per file. On other platforms it's unused: `inode_dev`
/// already gets `(inode, dev)` for free from the `stat()` call `WalkDir`
/// already makes.
///
/// `on_progress` is called every 200 entries with the running count so far.
/// There's no meaningful "total" on either path: computing one up front
/// would mean walking the tree twice, directly counter to the fact that the
/// slow part is the per-file work, not the (cheap) directory listing.
/// Progress is therefore indeterminate by design, not an oversight.
#[cfg(windows)]
pub fn scan_folder(
    root: &Path,
    medium_info: &medium::MediumInfo,
    mut on_progress: impl FnMut(u64),
) -> Result<Flattened, String> {
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }
    let refs_volume = medium_info
        .filesystem
        .as_deref()
        .is_some_and(|f| f.eq_ignore_ascii_case("refs"));
    let volume_dev = medium_info.volume_id.parse::<i64>().ok();

    match scan_folder_fast(root, refs_volume, volume_dev, &mut on_progress) {
        Ok(flat) => Ok(flat),
        Err(e) => {
            // Network path, permissions, or an OS that rejects the info
            // class: fall back to the portable `WalkDir` path for the
            // *whole* scan. Never fall back per-directory -- mixing the two
            // would produce inconsistent FileId semantics within one source.
            eprintln!(
                "fast directory enumeration failed for {} ({e}), falling back to the portable walker",
                root.display()
            );
            scan_folder_walkdir(root, on_progress)
        }
    }
}

#[cfg(not(windows))]
pub fn scan_folder(
    root: &Path,
    _medium_info: &medium::MediumInfo,
    on_progress: impl FnMut(u64),
) -> Result<Flattened, String> {
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }
    scan_folder_walkdir(root, on_progress)
}

/// Windows fast path: one `CreateFileW`/`GetFileInformationByHandleEx` call
/// per *directory* (see `scan_win::read_dir_ex`) instead of one `File::open`
/// per *entry*. On a large HDD tree (hundreds of thousands of files) that
/// removes hundreds of thousands of opens -- tens of minutes of pure seek
/// time. An explicit queue-based BFS (rather than `WalkDir`) also means
/// `parent_index` is resolved directly from the index just pushed, with no
/// `index_by_path` lookup table needed.
#[cfg(windows)]
fn scan_folder_fast(
    root: &Path,
    refs_volume: bool,
    volume_dev: Option<i64>,
    mut on_progress: impl FnMut(u64),
) -> std::io::Result<Flattened> {
    // A failure enumerating the root propagates to the caller, which falls
    // back to `scan_folder_walkdir` for the whole scan (see `scan_folder`'s
    // doc comment on why per-directory fallback is not an option).
    let root_entries = crate::scan_win::read_dir_ex(root, refs_volume)?;

    let mut flat = Flattened {
        nodes: Vec::new(),
        total_size: 0,
        file_count: 0,
        root_dev: volume_dev,
    };
    let mut scanned: u64 = 0;

    type QueueItem = (
        std::path::PathBuf,
        Option<usize>,
        String,
        i64,
        Vec<crate::scan_win::WinEntry>,
    );
    let mut queue: std::collections::VecDeque<QueueItem> = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), None, String::new(), 0, root_entries));

    while let Some((dir, parent_idx, rel_prefix, depth, entries)) = queue.pop_front() {
        for e in entries {
            scanned += 1;
            if scanned.is_multiple_of(200) {
                on_progress(scanned);
            }

            let rel_path = if rel_prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{rel_prefix}/{}", e.name)
            };
            let node_type = if e.is_dir {
                "directory"
            } else if e.is_reparse {
                "link"
            } else {
                "file"
            };
            let child_path = dir.join(&e.name);
            // Only non-directory reparse points (symlinks) get a target --
            // one `read_link` per symlink is negligible since they're rare.
            // The raw target is stored as-is: a dangling link's target is
            // still the fact worth recording, so this must never
            // canonicalize.
            let link_target = if e.is_reparse && !e.is_dir {
                std::fs::read_link(&child_path)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            };

            let size = e.size;
            let idx = flat.nodes.len();
            flat.nodes.push(FlatNode {
                parent_index: parent_idx,
                name: e.name,
                rel_path: rel_path.clone(),
                node_type: node_type.to_string(),
                size,
                mtime: e.mtime,
                inode: Some(e.file_id_low),
                dev: volume_dev,
                depth,
                subtree_size: 0,
                subtree_file_count: 0,
                inode_high: e.file_id_high,
                link_target,
            });

            // A count counts *names*: a symlink is something the user finds when listing
            // the directory, and something they must recreate at a destination, so it
            // counts. Its bytes do not -- a symlink holds no content of its own, so
            // `total_size`/`subtree_size` stay files-only.
            if node_type == "file" {
                flat.total_size += size;
            }
            if node_type == "file" || node_type == "link" {
                flat.file_count += 1;
            }

            // Reparse points are not descended into -- covers symlinks,
            // directory symlinks, and junctions, and prevents cycles.
            if e.is_dir && !e.is_reparse {
                if let Ok(child_entries) = crate::scan_win::read_dir_ex(&child_path, refs_volume) {
                    queue.push_back((child_path, Some(idx), rel_path, depth + 1, child_entries));
                }
                // A subdirectory enumeration failure (permissions, etc.)
                // just skips that subtree -- only a *root* failure (above)
                // triggers the whole-scan fallback.
            }
        }
    }

    on_progress(scanned);
    rollup(&mut flat);
    Ok(flat)
}

/// The portable path: a `WalkDir` traversal doing one `stat` per entry.
/// Used directly on non-Windows platforms, and as the Windows fallback when
/// `scan_folder_fast` can't enumerate the root at all.
fn scan_folder_walkdir(root: &Path, mut on_progress: impl FnMut(u64)) -> Result<Flattened, String> {
    let mut flat = Flattened {
        nodes: Vec::new(),
        total_size: 0,
        file_count: 0,
        root_dev: None,
    };

    // Map from absolute path -> index in `flat.nodes` for parent resolution.
    let mut index_by_path: HashMap<std::path::PathBuf, usize> = HashMap::new();

    let mut scanned: u64 = 0;
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        scanned += 1;
        if scanned.is_multiple_of(200) {
            on_progress(scanned);
        }
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();
        let rel_path = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| name.clone());
        let depth = (entry.depth() as i64) - 1;

        let file_type = entry.file_type();
        let node_type = if file_type.is_dir() {
            "directory"
        } else if file_type.is_symlink() {
            "link"
        } else {
            "file"
        };
        // Raw target, not canonicalized -- a dangling link's target is still
        // the fact worth recording.
        let link_target = if file_type.is_symlink() {
            std::fs::read_link(path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            None
        };

        let (inode, dev) = inode_dev(path, &meta);
        if flat.root_dev.is_none() {
            flat.root_dev = dev;
        }
        let size = meta.len() as i64;
        let mtime = meta.modified().ok().and_then(format_time);

        let parent_index = path.parent().and_then(|p| index_by_path.get(p).copied());

        let index = flat.nodes.len();
        index_by_path.insert(path.to_path_buf(), index);

        flat.nodes.push(FlatNode {
            parent_index,
            name,
            rel_path,
            node_type: node_type.to_string(),
            size,
            mtime,
            inode,
            dev,
            depth,
            subtree_size: 0,
            subtree_file_count: 0,
            inode_high: None,
            link_target,
        });

        if node_type == "file" {
            flat.total_size += size;
        }
        if node_type == "file" || node_type == "link" {
            flat.file_count += 1;
        }
    }
    on_progress(scanned);

    rollup(&mut flat);
    Ok(flat)
}

/// Roll up subtree totals. Unlike the parser, scan order (a walk) does not
/// guarantee children follow parents contiguously, so we accumulate via parent
/// links in a second pass ordered by descending depth.
fn rollup(flat: &mut Flattened) {
    // First give files their own contribution.
    for n in flat.nodes.iter_mut() {
        if n.node_type == "file" {
            n.subtree_size = n.size;
        }
        if n.node_type == "file" || n.node_type == "link" {
            n.subtree_file_count = 1;
        }
    }
    // Process deepest nodes first so parents accumulate complete child totals.
    let mut order: Vec<usize> = (0..flat.nodes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(flat.nodes[i].depth));
    for i in order {
        let (st_size, st_count, parent) = {
            let n = &flat.nodes[i];
            (n.subtree_size, n.subtree_file_count, n.parent_index)
        };
        if let Some(p) = parent {
            let pn = &mut flat.nodes[p];
            pn.subtree_size += st_size;
            pn.subtree_file_count += st_count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_medium() -> medium::MediumInfo {
        medium::MediumInfo {
            medium_kind: medium::MediumKind::Unknown,
            filesystem: None,
            volume_id: String::new(),
        }
    }

    #[test]
    fn symlink_target_is_recorded() {
        let dir = std::env::temp_dir().join(format!(
            "archive-dedup-scan-symlink-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real.txt");
        std::fs::write(&target, b"hello").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real.txt", dir.join("link.txt")).unwrap();
            std::os::unix::fs::symlink("does-not-exist.txt", dir.join("dangling.txt")).unwrap();
        }

        // `SeCreateSymbolicLinkPrivilege` is off by default outside
        // Developer Mode / an elevated shell -- that's a machine setting,
        // not a bug (see `windows_tests::reparse_points_are_not_descended`,
        // which hits the identical `ERROR_PRIVILEGE_NOT_HELD`). Skip rather
        // than fail so this test still exercises the real thing on machines
        // that do have the privilege, without going red on ones that don't.
        #[cfg(windows)]
        {
            if let Err(e) = std::os::windows::fs::symlink_file("real.txt", dir.join("link.txt")) {
                if e.raw_os_error() == Some(1314) {
                    eprintln!(
                        "skipping symlink_target_is_recorded: {e} (enable Developer Mode or run elevated to create symlinks)"
                    );
                    std::fs::remove_dir_all(&dir).ok();
                    return;
                }
                panic!("failed to create test symlink: {e}");
            }
            std::os::windows::fs::symlink_file("does-not-exist.txt", dir.join("dangling.txt"))
                .unwrap();
        }

        let flat = scan_folder(&dir, &no_medium(), |_| {}).unwrap();

        let link = flat.nodes.iter().find(|n| n.name == "link.txt").unwrap();
        assert_eq!(link.node_type, "link");
        assert_eq!(link.link_target.as_deref(), Some("real.txt"));

        let dangling = flat
            .nodes
            .iter()
            .find(|n| n.name == "dangling.txt")
            .unwrap();
        assert_eq!(
            dangling.link_target.as_deref(),
            Some("does-not-exist.txt"),
            "a dangling symlink's target must still be recorded, not skipped"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Windows-only: cannot be built or run outside a real Windows target, so
/// these never execute in this (Linux) sandbox. They're kept here rather
/// than deleted because they're the actual verification for Stage 4's
/// riskiest claim -- that `scan_folder_fast` and `scan_folder_walkdir`
/// agree.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::fs;

    fn build_test_tree(dir: &Path) {
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"hello").unwrap();
        fs::write(dir.join("sub/b.txt"), b"world!!").unwrap();
    }

    /// The test that matters most for this stage: run both implementations
    /// over the same tree and assert they agree on every
    /// `(rel_path, type, size, mtime)` triple. Sizes and mtimes are where a
    /// `FILETIME`- or `EndOfFile`-handling mistake in `scan_win.rs` would
    /// surface.
    #[test]
    fn scan_produces_identical_flat_nodes_as_the_walkdir_path() {
        let dir = std::env::temp_dir().join(format!("scan-parity-test-{}", std::process::id()));
        build_test_tree(&dir);

        let fast = scan_folder_fast(&dir, false, Some(1), |_| {}).unwrap();
        let walk = scan_folder_walkdir(&dir, |_| {}).unwrap();

        let mut fast_keys: Vec<(String, String, i64, Option<String>)> = fast
            .nodes
            .iter()
            .map(|n| {
                (
                    n.rel_path.clone(),
                    n.node_type.clone(),
                    n.size,
                    n.mtime.clone(),
                )
            })
            .collect();
        let mut walk_keys: Vec<(String, String, i64, Option<String>)> = walk
            .nodes
            .iter()
            .map(|n| {
                (
                    n.rel_path.clone(),
                    n.node_type.clone(),
                    n.size,
                    n.mtime.clone(),
                )
            })
            .collect();
        fast_keys.sort();
        walk_keys.sort();
        assert_eq!(fast_keys, walk_keys);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reparse_points_are_not_descended() {
        let dir = std::env::temp_dir().join(format!("scan-reparse-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("real_target/inner")).unwrap();
        fs::write(dir.join("real_target/inner/x.txt"), b"x").unwrap();
        std::os::windows::fs::symlink_dir(dir.join("real_target"), dir.join("link_to_target"))
            .unwrap();

        let flat = scan_folder_fast(&dir, false, Some(1), |_| {}).unwrap();

        assert!(
            flat.nodes.iter().any(|n| n.rel_path == "link_to_target"),
            "the reparse point itself must still be recorded"
        );
        assert!(
            flat.nodes
                .iter()
                .all(|n| !n.rel_path.starts_with("link_to_target/")),
            "a directory symlink/junction must never be descended into"
        );

        fs::remove_dir_all(&dir).ok();
    }
}
