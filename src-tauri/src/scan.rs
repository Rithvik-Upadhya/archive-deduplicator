//! Live folder scanning as a `tree`-independent alternative. Produces the same
//! `Flattened` structure as `parse.rs` so both import paths share insertion.

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

/// Extract the inode and device id in a cross-platform way.
#[cfg(unix)]
fn inode_dev(_path: &Path, meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    use std::os::unix::fs::MetadataExt;
    (Some(meta.ino() as i64), Some(meta.dev() as i64))
}

/// On Windows, `Metadata::file_index`/`volume_serial_number` are still
/// gated behind the unstable `windows_by_handle` feature on stable Rust,
/// so query the same info manually via `GetFileInformationByHandle`.
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
pub fn scan_folder(root: &Path) -> Result<Flattened, String> {
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", root.display()));
    }

    let mut flat = Flattened {
        nodes: Vec::new(),
        total_size: 0,
        file_count: 0,
        root_dev: None,
    };

    // Map from absolute path -> index in `flat.nodes` for parent resolution.
    let mut index_by_path: HashMap<std::path::PathBuf, usize> = HashMap::new();

    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
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
        });

        if node_type == "file" {
            flat.total_size += size;
            flat.file_count += 1;
        }
    }

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
