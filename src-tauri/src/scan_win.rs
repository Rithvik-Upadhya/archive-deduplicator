//! Windows-only fast directory enumeration via `GetFileInformationByHandleEx`.
//!
//! `scan.rs`'s original Windows path did one `File::open` +
//! `GetFileInformationByHandle` per *entry* (see its old `inode_dev`) just to
//! recover a file id and volume serial. On a large HDD tree (hundreds of
//! thousands of files) that's hundreds of thousands of opens -- tens of
//! minutes of pure seek time. This module does one `CreateFileW` per
//! *directory* instead, and pulls every child's id/size/mtime out of the
//! same buffered enumeration call.
//!
//! Cannot be built or tested outside a Windows target -- this whole module is
//! `cfg(windows)`-gated at its declaration site in `lib.rs`.

use chrono::{DateTime, Local, Utc};
use std::ffi::c_void;
use std::io;
use std::mem::offset_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::null;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_128, FILE_ID_BOTH_DIR_INFO, FILE_ID_EXTD_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdExtdDirectoryInfo,
    FileIdExtdDirectoryRestartInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
};

/// One directory child as reported by `GetFileInformationByHandleEx`.
pub struct WinEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_reparse: bool,
    pub size: i64,
    pub mtime: Option<String>,
    pub file_id_low: i64,
    /// Upper 64 bits of a ReFS `FILE_ID_128`. `None` on NTFS/other
    /// filesystems, which only ever hand out a 64-bit file id.
    pub file_id_high: Option<i64>,
}

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `FILETIME` (100-ns ticks since 1601-01-01 UTC) -> the same
/// `%Y-%m-%d_%H:%M:%S` local-time string `scan.rs::format_time` produces, so
/// scanned and JSON-imported mtimes stay comparable.
fn filetime_to_tree_string(ft: i64) -> Option<String> {
    const EPOCH_DIFF_100NS: i64 = 116_444_736_000_000_000;
    if ft <= 0 {
        return None;
    }
    let unix_100ns = ft - EPOCH_DIFF_100NS;
    let dt = DateTime::<Utc>::from_timestamp(
        unix_100ns / 10_000_000,
        ((unix_100ns % 10_000_000) * 100) as u32,
    )?;
    Some(
        dt.with_timezone(&Local)
            .format("%Y-%m-%d_%H:%M:%S")
            .to_string(),
    )
}

/// Read every entry of `dir` in one open. `refs_volume` selects the
/// `FileIdExtd*` info class (128-bit ids) over `FileIdBoth*` (64-bit) --
/// callers get this from `medium::MediumInfo.filesystem`.
pub fn read_dir_ex(dir: &Path, refs_volume: bool) -> io::Result<Vec<WinEntry>> {
    let wide_path = to_wide(dir);
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let result = read_dir_ex_inner(handle, refs_volume);
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn read_dir_ex_inner(h: HANDLE, refs_volume: bool) -> io::Result<Vec<WinEntry>> {
    let mut entries = Vec::new();
    // The records contain LARGE_INTEGER (i64) fields -- a misaligned Vec<u8>
    // buffer would produce unaligned reads through the struct casts below, so
    // allocate as Vec<u64> (8-byte aligned) and reinterpret as bytes. 8192 *
    // 8 = 64 KiB.
    let mut buf: Vec<u64> = vec![0; 8192];
    let ptr = buf.as_mut_ptr() as *mut u8;
    let cap = buf.len() * 8;

    let (restart_class, cont_class) = if refs_volume {
        (FileIdExtdDirectoryRestartInfo, FileIdExtdDirectoryInfo)
    } else {
        (FileIdBothDirectoryRestartInfo, FileIdBothDirectoryInfo)
    };

    let mut class = restart_class;
    loop {
        let ok =
            unsafe { GetFileInformationByHandleEx(h, class, ptr as *mut c_void, cap as u32) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                break;
            }
            return Err(io::Error::last_os_error());
        }
        class = cont_class;

        let mut off = 0usize;
        loop {
            let record = if refs_volume {
                unsafe { read_extd_record(ptr, off) }
            } else {
                unsafe { read_both_record(ptr, off) }
            };

            if record.name != "." && record.name != ".." {
                entries.push(WinEntry {
                    name: record.name,
                    is_dir: record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                    is_reparse: record.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                    size: record.end_of_file,
                    mtime: filetime_to_tree_string(record.last_write_time),
                    file_id_low: record.file_id_low,
                    file_id_high: record.file_id_high,
                });
            }

            if record.next_entry_offset == 0 {
                break;
            }
            off += record.next_entry_offset as usize;
        }
    }

    Ok(entries)
}

struct RawRecord {
    next_entry_offset: u32,
    attributes: u32,
    end_of_file: i64,
    last_write_time: i64,
    file_id_low: i64,
    file_id_high: Option<i64>,
    name: String,
}

/// # Safety
/// `ptr.add(off)` must point at a live `FILE_ID_BOTH_DIR_INFO` record inside
/// a buffer of at least `off + size_of::<FILE_ID_BOTH_DIR_INFO>() +
/// FileNameLength` bytes, 8-byte aligned (guaranteed by the `Vec<u64>`
/// allocation in `read_dir_ex_inner`).
unsafe fn read_both_record(ptr: *const u8, off: usize) -> RawRecord {
    unsafe {
        let base = ptr.add(off);
        let rec = &*(base as *const FILE_ID_BOTH_DIR_INFO);
        // FileNameLength is in BYTES, not UTF-16 code units.
        let name_len = rec.FileNameLength as usize / 2;
        let name_ptr = base.add(offset_of!(FILE_ID_BOTH_DIR_INFO, FileName)) as *const u16;
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(name_ptr, name_len));
        RawRecord {
            next_entry_offset: rec.NextEntryOffset,
            attributes: rec.FileAttributes,
            end_of_file: rec.EndOfFile,
            last_write_time: rec.LastWriteTime,
            file_id_low: rec.FileId,
            file_id_high: None,
            name,
        }
    }
}

/// # Safety
/// Same contract as [`read_both_record`], for `FILE_ID_EXTD_DIR_INFO`
/// (ReFS's 128-bit file ids) instead.
unsafe fn read_extd_record(ptr: *const u8, off: usize) -> RawRecord {
    unsafe {
        let base = ptr.add(off);
        let rec = &*(base as *const FILE_ID_EXTD_DIR_INFO);
        let name_len = rec.FileNameLength as usize / 2;
        let name_ptr = base.add(offset_of!(FILE_ID_EXTD_DIR_INFO, FileName)) as *const u16;
        let name = String::from_utf16_lossy(std::slice::from_raw_parts(name_ptr, name_len));
        let (low, high) = split_file_id_128(&rec.FileId);
        RawRecord {
            next_entry_offset: rec.NextEntryOffset,
            attributes: rec.FileAttributes,
            end_of_file: rec.EndOfFile,
            last_write_time: rec.LastWriteTime,
            file_id_low: low,
            file_id_high: Some(high),
            name,
        }
    }
}

/// Split a `FILE_ID_128` (16 raw bytes, little-endian halves) into
/// `(low: i64, high: i64)` for `nodes.inode` / `nodes.inode_high`.
fn split_file_id_128(id: &FILE_ID_128) -> (i64, i64) {
    let mut low_bytes = [0u8; 8];
    let mut high_bytes = [0u8; 8];
    low_bytes.copy_from_slice(&id.Identifier[0..8]);
    high_bytes.copy_from_slice(&id.Identifier[8..16]);
    (
        i64::from_le_bytes(low_bytes),
        i64::from_le_bytes(high_bytes),
    )
}

// These tests only compile and run on a real Windows target -- there is no
// way to exercise `CreateFileW`/`GetFileInformationByHandleEx` on Linux, so
// they cannot be run in this sandbox. `scan.rs`'s
// `scan_produces_identical_flat_nodes_as_the_walkdir_path` and
// `reparse_points_are_not_descended` tests live there instead, since they
// need to drive the BFS walker rather than `read_dir_ex` directly.
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn read_dir_ex_returns_every_entry_with_ids_and_sizes() {
        let dir = std::env::temp_dir().join(format!("scan-win-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..300 {
            fs::write(dir.join(format!("file-{i}.txt")), b"hello").unwrap();
        }

        let entries = read_dir_ex(&dir, false).unwrap();
        assert_eq!(entries.len(), 300);
        for e in &entries {
            assert!(!e.is_dir);
            assert_eq!(e.size, 5);
            assert!(e.mtime.is_some());
            assert!(e.file_id_low != 0);
            assert!(e.file_id_high.is_none(), "NTFS has no 128-bit file id");
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_dir_ex_pages_correctly_past_one_buffer() {
        // Long names to force more than one 64 KiB GetFileInformationByHandleEx
        // call -- the restart-vs-continue class switch and the
        // NextEntryOffset walk both have to survive a page boundary.
        let dir = std::env::temp_dir().join(format!("scan-win-page-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let long_stem = "x".repeat(200);
        for i in 0..500 {
            fs::write(dir.join(format!("{long_stem}-{i}.txt")), b"y").unwrap();
        }

        let entries = read_dir_ex(&dir, false).unwrap();
        assert_eq!(entries.len(), 500, "every entry across all buffer pages must be returned");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_dir_ex_skips_dot_entries() {
        let dir = std::env::temp_dir().join(format!("scan-win-dot-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), b"x").unwrap();

        let entries = read_dir_ex(&dir, false).unwrap();
        assert!(entries.iter().all(|e| e.name != "." && e.name != ".."));
        assert_eq!(entries.len(), 1);

        fs::remove_dir_all(&dir).ok();
    }
}
