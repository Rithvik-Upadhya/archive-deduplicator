//! Storage-medium (HDD/SSD) and filesystem-type detection.
//!
//! Two independent things feed off this: the read strategy used by the
//! hashing pipeline (queue depth, physical-order sorting, sampling defaults)
//! and the trust decision `links.rs` makes before collapsing hardlink alias
//! sets (only certain filesystems hand out stable, collision-safe file IDs).
//!
//! Detection never fails outright -- on any platform-API error or ambiguity
//! it degrades to `MediumKind::Unknown`/`filesystem: None`. `profile_for`
//! treats every non-SSD kind (including `Unknown`) as HDD-conservative,
//! because the cost of guessing wrong is asymmetric: HDD settings on an SSD
//! only leave some throughput on the table, but SSD settings on a real HDD
//! thrash the head. Detection failures should always fall the safe way.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediumKind {
    Hdd,
    Ssd,
    Network,
    Optical,
    Unknown,
}

impl MediumKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediumKind::Hdd => "hdd",
            MediumKind::Ssd => "ssd",
            MediumKind::Network => "network",
            MediumKind::Optical => "optical",
            MediumKind::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hdd" => Some(MediumKind::Hdd),
            "ssd" => Some(MediumKind::Ssd),
            "network" => Some(MediumKind::Network),
            "optical" => Some(MediumKind::Optical),
            "unknown" => Some(MediumKind::Unknown),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediumInfo {
    pub medium_kind: MediumKind,
    pub filesystem: Option<String>,
    pub volume_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediumProfile {
    pub reader_queue_depth: usize,
    pub sort_by_physical_order: bool,
    pub hash_min_size_default: i64,
    pub sampling_default: bool,
}

/// §4's per-medium settings table. Every non-SSD kind gets the conservative
/// HDD profile -- see the module doc for why "assume HDD" is the safe
/// default rather than a middle ground.
pub fn profile_for(kind: MediumKind) -> MediumProfile {
    match kind {
        MediumKind::Ssd => MediumProfile {
            reader_queue_depth: 32,
            sort_by_physical_order: false,
            hash_min_size_default: 0,
            sampling_default: false,
        },
        MediumKind::Hdd | MediumKind::Network | MediumKind::Optical | MediumKind::Unknown => {
            MediumProfile {
                reader_queue_depth: 4,
                sort_by_physical_order: true,
                hash_min_size_default: 65536,
                sampling_default: true,
            }
        }
    }
}

/// Filesystems that hand out a stable, collision-safe per-file identifier
/// (inode / MFT reference number / etc.) -- the gate `links.rs` uses before
/// trusting `(dev, inode)` collisions as real hardlinks. FAT32/exFAT (common
/// on the external/USB media this app targets) synthesize an identifier that
/// isn't stable across renames, so they -- and anything unrecognized -- stay
/// untrusted, matching the app's false-positive-averse default.
const TRUSTED_FILESYSTEMS: &[&str] = &[
    "ntfs", "refs", "ext2", "ext3", "ext4", "xfs", "btrfs", "apfs", "hfs+", "hfsplus", "zfs",
];

pub fn filesystem_is_trusted(fs: Option<&str>) -> bool {
    match fs {
        Some(fs) => TRUSTED_FILESYSTEMS.contains(&fs.to_lowercase().as_str()),
        None => false,
    }
}

pub fn detect(root: &Path) -> MediumInfo {
    platform::detect(root)
}

// Windows' `detect()` always resolves to a concrete `Network` or
// drive-based kind and never needs the fully-generic "give up" shape --
// unlike Linux/macOS, whose `detect()` falls back to this on I/O errors --
// so this is legitimately unused on that platform, not an oversight.
#[cfg_attr(windows, allow(dead_code))]
fn unknown() -> MediumInfo {
    MediumInfo {
        medium_kind: MediumKind::Unknown,
        filesystem: None,
        volume_id: String::new(),
    }
}

// ---------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------
#[cfg(target_os = "linux")]
mod platform {
    use super::{MediumInfo, MediumKind, unknown};
    use std::path::{Path, PathBuf};

    pub fn detect(root: &Path) -> MediumInfo {
        let dev = match std::fs::metadata(root) {
            Ok(m) => {
                use std::os::unix::fs::MetadataExt;
                m.dev()
            }
            Err(_) => return unknown(),
        };
        let major = libc::major(dev);
        let minor = libc::minor(dev);

        let medium_kind = detect_rotational(major, minor).unwrap_or(MediumKind::Hdd);
        let filesystem = detect_fs_type(root);

        MediumInfo {
            medium_kind,
            filesystem,
            volume_id: dev.to_string(),
        }
    }

    /// Resolve `/sys/dev/block/<major>:<minor>`, then walk **up** looking for
    /// a `queue/` subdirectory -- for a partition it lives on the parent
    /// disk, and device-mapper/LVM/loop devices can need a couple more hops.
    /// Bounded both by `/sys/devices` (the spec's stated stopping point) and
    /// a hard iteration cap as a defensive backstop against a pathological
    /// symlink chain.
    fn detect_rotational(major: u32, minor: u32) -> Option<MediumKind> {
        let sys_path = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
        let mut dir = std::fs::canonicalize(&sys_path).ok()?;

        for _ in 0..16 {
            if dir.join("queue").is_dir() {
                let rotational = std::fs::read_to_string(dir.join("queue/rotational")).ok()?;
                return match rotational.trim() {
                    "1" => Some(MediumKind::Hdd),
                    "0" => Some(MediumKind::Ssd),
                    _ => None,
                };
            }
            if dir == Path::new("/sys/devices") || !dir.pop() {
                return None;
            }
        }
        None
    }

    /// Match `root` against `/proc/mounts` by longest mount-point prefix,
    /// preferred over `statfs().f_type` magic-number matching since the type
    /// string there is already a human-readable name ("ntfs3", "vfat",
    /// "exfat", "ext4", ...) with no lookup table to get wrong.
    fn detect_fs_type(root: &Path) -> Option<String> {
        let canon = std::fs::canonicalize(root).ok()?;
        let mounts = std::fs::read_to_string("/proc/mounts").ok()?;

        let mut best: Option<(usize, String)> = None;
        for line in mounts.lines() {
            let mut parts = line.split_whitespace();
            let _device = parts.next()?;
            let mount_point = parts.next()?;
            let fs_type = parts.next()?;
            if canon.starts_with(mount_point) {
                let len = mount_point.len();
                if best.as_ref().is_none_or(|(l, _)| len > *l) {
                    best = Some((len, fs_type.to_string()));
                }
            }
        }
        best.map(|(_, fs)| normalize_fs_name(&fs))
    }

    /// `fuseblk` (the generic FUSE-block type reported for both ntfs-3g and
    /// some exFAT drivers) is deliberately left unmapped: normalizing it to
    /// "NTFS" would risk misreporting an exFAT volume as trusted, exactly
    /// the kind of false positive this app is designed to avoid.
    fn normalize_fs_name(fs: &str) -> String {
        match fs {
            "vfat" | "msdos" => "FAT32".to_string(),
            "exfat" => "exFAT".to_string(),
            "ntfs" | "ntfs3" => "NTFS".to_string(),
            other => other.to_string(),
        }
    }
}

// ---------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod platform {
    use super::{MediumInfo, MediumKind, unknown};
    use std::path::Path;
    use std::process::Command;

    pub fn detect(root: &Path) -> MediumInfo {
        let Ok(path_c) = std::ffi::CString::new(root.to_string_lossy().as_bytes()) else {
            return unknown();
        };
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path_c.as_ptr(), &mut stat) } != 0 {
            return unknown();
        }

        let filesystem = normalize_fs_name(&cstr_array_to_string(&stat.f_fstypename));
        let mntfromname = cstr_array_to_string(&stat.f_mntfromname);
        let medium_kind = detect_medium_via_diskutil(&mntfromname);

        MediumInfo {
            medium_kind,
            filesystem: if filesystem.is_empty() {
                None
            } else {
                Some(filesystem)
            },
            volume_id: mntfromname,
        }
    }

    fn cstr_array_to_string(buf: &[std::os::raw::c_char]) -> String {
        let bytes: Vec<u8> = buf
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn normalize_fs_name(fs: &str) -> String {
        match fs {
            "msdos" => "FAT32".to_string(),
            "exfat" => "exFAT".to_string(),
            "hfs" => "HFS+".to_string(),
            "apfs" => "APFS".to_string(),
            other => other.to_string(),
        }
    }

    /// No raw IOKit calls (`kIOPropertyDeviceCharacteristicsKey`) -- per the
    /// spec's own explicitly-allowed v1 simplification, shell out to
    /// `diskutil info` instead. Prefer its direct `Solid State:` field when
    /// present (more accurate than an internal/external guess); fall back to
    /// `Internal:` (SSD assumed for internal, HDD for external, exactly the
    /// spec's stated v1 default) when it isn't. Any failure defaults to HDD,
    /// consistent with the module's asymmetric-risk default.
    fn detect_medium_via_diskutil(device: &str) -> MediumKind {
        let output = match Command::new("diskutil").arg("info").arg(device).output() {
            Ok(o) if o.status.success() => o,
            _ => return MediumKind::Hdd,
        };
        let text = String::from_utf8_lossy(&output.stdout);

        let mut solid_state: Option<bool> = None;
        let mut internal: Option<bool> = None;
        for line in text.lines() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("Solid State:") {
                solid_state = Some(v.trim().eq_ignore_ascii_case("yes"));
            } else if let Some(v) = l.strip_prefix("Internal:") {
                internal = Some(v.trim().eq_ignore_ascii_case("yes"));
            }
        }
        match solid_state.or(internal) {
            Some(true) => MediumKind::Ssd,
            _ => MediumKind::Hdd,
        }
    }
}

// ---------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod platform {
    use super::{MediumInfo, MediumKind};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationW,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Only handles a drive-letter root (`C:\...`); UNC/network paths fall
    /// through to `Network` since there's no single local volume to query.
    fn drive_letter(root: &Path) -> Option<char> {
        let s = root.to_string_lossy();
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            Some(bytes[0] as char)
        } else {
            None
        }
    }

    pub fn detect(root: &Path) -> MediumInfo {
        let Some(letter) = drive_letter(root) else {
            return MediumInfo {
                medium_kind: MediumKind::Network,
                filesystem: None,
                volume_id: String::new(),
            };
        };
        let volume_path = format!(r"\\.\{letter}:");
        let mount_root = format!("{letter}:\\");

        let (filesystem, volume_id) =
            query_volume_info(&mount_root).unwrap_or((None, String::new()));
        let medium_kind = query_seek_penalty(&volume_path).unwrap_or(MediumKind::Hdd);

        MediumInfo {
            medium_kind,
            filesystem,
            volume_id,
        }
    }

    fn query_volume_info(mount_root: &str) -> Option<(Option<String>, String)> {
        let wide_root = to_wide(mount_root);
        let mut fs_name = [0u16; 64];
        let mut serial: u32 = 0;
        let ok = unsafe {
            GetVolumeInformationW(
                wide_root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs_name.as_mut_ptr(),
                fs_name.len() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        let len = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        let fs = String::from_utf16_lossy(&fs_name[..len]);
        Some((
            if fs.is_empty() { None } else { Some(fs) },
            serial.to_string(),
        ))
    }

    /// `IOCTL_STORAGE_QUERY_PROPERTY` / `StorageDeviceSeekPenaltyProperty`
    /// needs only a volume handle opened with `dwDesiredAccess = 0` -- no
    /// admin rights required. Works when the volume has a single extent,
    /// which covers essentially all external/USB media; a multi-extent
    /// volume (spanned/striped) falls back to the `None` -> HDD default
    /// rather than following the `\\.\PhysicalDriveN` indirection, which
    /// this v1 does not implement.
    fn query_seek_penalty(volume_path: &str) -> Option<MediumKind> {
        let wide_vol = to_wide(volume_path);
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide_vol.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }

        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut descriptor: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
        let mut bytes_returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                &query as *const _ as *const _,
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                &mut descriptor as *mut _ as *mut _,
                std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut bytes_returned,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        Some(if descriptor.IncursSeekPenalty == 0 {
            MediumKind::Ssd
        } else {
            MediumKind::Hdd
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::{MediumInfo, unknown};
    use std::path::Path;

    pub fn detect(_root: &Path) -> MediumInfo {
        unknown()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_filesystems_are_trusted_case_insensitively() {
        for fs in [
            "NTFS", "ntfs", "ReFS", "ext2", "ext3", "ext4", "XFS", "Btrfs", "APFS", "HFS+", "ZFS",
        ] {
            assert!(filesystem_is_trusted(Some(fs)), "{fs} should be trusted");
        }
    }

    #[test]
    fn fat_and_unknown_filesystems_are_not_trusted() {
        assert!(!filesystem_is_trusted(Some("FAT32")));
        assert!(!filesystem_is_trusted(Some("exFAT")));
        assert!(!filesystem_is_trusted(Some("fuseblk")));
        assert!(!filesystem_is_trusted(Some("made-up-fs")));
        assert!(!filesystem_is_trusted(None));
    }

    #[test]
    fn ssd_profile_matches_the_spec_table() {
        let p = profile_for(MediumKind::Ssd);
        assert_eq!(p.reader_queue_depth, 32);
        assert!(!p.sort_by_physical_order);
        assert_eq!(p.hash_min_size_default, 0);
        assert!(!p.sampling_default);
    }

    #[test]
    fn hdd_profile_matches_the_spec_table() {
        let p = profile_for(MediumKind::Hdd);
        assert_eq!(p.reader_queue_depth, 4);
        assert!(p.sort_by_physical_order);
        assert_eq!(p.hash_min_size_default, 65536);
        assert!(p.sampling_default);
    }

    #[test]
    fn unknown_network_and_optical_all_get_the_conservative_hdd_profile() {
        let hdd = profile_for(MediumKind::Hdd);
        for kind in [
            MediumKind::Unknown,
            MediumKind::Network,
            MediumKind::Optical,
        ] {
            assert_eq!(profile_for(kind), hdd);
        }
    }

    #[test]
    fn medium_kind_round_trips_through_as_str_and_parse() {
        for kind in [
            MediumKind::Hdd,
            MediumKind::Ssd,
            MediumKind::Network,
            MediumKind::Optical,
            MediumKind::Unknown,
        ] {
            assert_eq!(MediumKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MediumKind::parse("nonsense"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rotational_walk_up_finds_queue_dir_on_a_parent() {
        // Mimic /sys/dev/block/<maj>:<min> resolving to a partition whose
        // queue/ lives one level up, on the parent disk -- the exact shape
        // detect_rotational's upward walk exists to handle.
        let tmp = std::env::temp_dir().join(format!("adedup_sysfs_test_{}", std::process::id()));
        let disk = tmp.join("sda");
        let partition = disk.join("sda1");
        std::fs::create_dir_all(disk.join("queue")).unwrap();
        std::fs::create_dir_all(&partition).unwrap();
        std::fs::write(disk.join("queue/rotational"), "1\n").unwrap();

        // Exercise the same upward-walk logic directly against our fake tree
        // (can't route through real major:minor numbers in a unit test).
        let mut dir = partition.clone();
        let mut found = None;
        for _ in 0..16 {
            if dir.join("queue").is_dir() {
                found = std::fs::read_to_string(dir.join("queue/rotational")).ok();
                break;
            }
            if !dir.pop() {
                break;
            }
        }
        assert_eq!(found.as_deref(), Some("1\n"));

        std::fs::remove_dir_all(&tmp).ok();
    }
}
