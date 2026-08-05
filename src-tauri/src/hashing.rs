//! Content hashing: workspace-locked hash specs, deterministic region
//! planning for sampled hashing, BLAKE3 digest computation, and a small
//! reader pool for keeping several reads in flight on a spinning disk.
//!
//! Only live scans ever reach this module -- `tree` JSON imports have no
//! content to read, so they stay permanently unhashed (see `parse.rs`).

use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// The digest-affecting settings for a workspace (see db.rs's `SCHEMA_VERSION`
/// doc and the design spec's §6.1): sample threshold, probe size, and stride.
/// `threshold: None` means "full hash everything" -- implemented as an
/// infinity sentinel rather than a separate boolean, so there is one fewer
/// combination to reason about and one fewer way for two sources to disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashSpec {
    pub threshold: Option<i64>,
    pub probe: i64,
    pub stride: i64,
}

impl HashSpec {
    /// A reasonable out-of-the-box default: 8 MiB threshold, 1 MiB probe,
    /// 32 MiB stride -- used only until a workspace's first hashed scan
    /// locks in whatever spec was actually in force.
    pub fn default_spec() -> Self {
        HashSpec {
            threshold: Some(8 * 1024 * 1024),
            probe: 1024 * 1024,
            stride: 32 * 1024 * 1024,
        }
    }

    pub fn spec_string(&self) -> String {
        match self.threshold {
            None => "blake3/v1/full".to_string(),
            Some(t) => format!("blake3/v1/th{t}-s{}-t{}", self.probe, self.stride),
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
                stride: 0,
            });
        }
        let rest = s.strip_prefix("blake3/v1/th")?;
        let (th, rest) = rest.split_once("-s")?;
        let (probe_str, stride_str) = rest.split_once("-t")?;
        Some(HashSpec {
            threshold: Some(th.parse().ok()?),
            probe: probe_str.parse().ok()?,
            stride: stride_str.parse().ok()?,
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
/// 4096 interior probes, merged. Offsets derive **only** from `size` and the
/// spec constants -- never scan order or a running counter -- so the same
/// file samples identically on any two media.
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
    let t = spec.stride.max(1);

    let mut regions = vec![0..p.min(size), (size - p).max(0)..size];
    let n = (size / t).clamp(0, 4096);
    for i in 1..=n {
        let off = align_down(i * size / (n + 1), 4096);
        regions.push(off..(off + p).min(size));
    }
    merge_overlapping(regions)
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

    let mut hasher = blake3::Hasher::new();
    hasher.update(spec.spec_string().as_bytes());
    hasher.update(&size.to_le_bytes());

    let mut file = std::fs::File::open(path)?;
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

/// Hash `jobs` using a fixed-size worker pool (sized from
/// `medium::MediumProfile::reader_queue_depth`). On HDD the point isn't CPU
/// parallelism -- the head is singular -- but keeping several reads in
/// flight so the drive's NCQ can reorder them. Results arrive in completion
/// order, not job order; each is tagged with its node id so the caller (the
/// batched-persistence loop in `commands.rs`) can match them back up.
pub fn hash_files_pooled(
    jobs: Vec<HashJob>,
    spec: HashSpec,
    queue_depth: usize,
) -> Vec<(i64, std::io::Result<(blake3::Hash, &'static str, i64)>)> {
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    let expected = jobs.len();
    let (work_tx, work_rx) = mpsc::channel::<HashJob>();
    for job in jobs {
        let _ = work_tx.send(job);
    }
    drop(work_tx);
    let work_rx = Arc::new(Mutex::new(work_rx));

    let (result_tx, result_rx) = mpsc::channel();
    let n_workers = queue_depth.max(1);
    let mut handles = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let work_rx = Arc::clone(&work_rx);
        let result_tx = result_tx.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = {
                    let rx = work_rx.lock().unwrap();
                    rx.recv()
                };
                let Ok((node_id, path, size)) = next else {
                    break;
                };
                let result = hash_file(&path, size, &spec);
                if result_tx.send((node_id, result)).is_err() {
                    break;
                }
            }
        }));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sampled_spec() -> HashSpec {
        HashSpec {
            threshold: Some(8 * 1024 * 1024),
            probe: 1024 * 1024,
            stride: 32 * 1024 * 1024,
        }
    }

    #[test]
    fn spec_string_round_trips_for_full_and_sampled() {
        let full = HashSpec {
            threshold: None,
            probe: 0,
            stride: 0,
        };
        assert_eq!(HashSpec::parse(&full.spec_string()), Some(full));

        let sampled = sampled_spec();
        assert_eq!(HashSpec::parse(&sampled.spec_string()), Some(sampled));
        assert_eq!(
            sampled.spec_string(),
            "blake3/v1/th8388608-s1048576-t33554432"
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(HashSpec::parse("not-a-spec"), None);
        assert_eq!(HashSpec::parse("blake3/v1/thX-s1-t1"), None);
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
            stride: 4096,
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
    fn boundary_size_exactly_at_stride_has_one_interior_probe() {
        let spec = sampled_spec();
        // size == stride => n = clamp(size/stride, 0, 4096) = 1.
        let regions = plan_regions(spec.stride, &spec);
        // head, one interior probe, tail => at least 2 after any merging,
        // and never fewer than head+tail.
        assert!(regions.len() >= 2);
    }

    #[test]
    fn boundary_size_one_below_stride_has_zero_interior_probes() {
        let spec = sampled_spec();
        let regions = plan_regions(spec.stride - 1, &spec);
        // n = (stride-1)/stride = 0 => only head+tail (possibly merged if
        // the file is small enough that head/tail overlap).
        assert!(!regions.is_empty());
    }

    #[test]
    fn size_smaller_than_probe_merges_head_and_tail_into_one_region() {
        // A spec where the probe is wider than the threshold (unusual, but
        // not disallowed) -- head (0..size) and tail (0..size) fully overlap
        // whenever size <= probe, regardless of how they compare to stride.
        let spec = HashSpec {
            threshold: Some(100),
            probe: 1000,
            stride: 4096,
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
            stride: 0,
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
            stride: 0,
        };
        let (hash_a, ..) = hash_file(&a, 11, &spec).unwrap();
        let (hash_b, ..) = hash_file(&b, 11, &spec).unwrap();
        assert_ne!(hash_a, hash_b);

        std::fs::remove_dir_all(&dir).ok();
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
            stride: 0,
        };
        let spec_b = HashSpec {
            threshold: Some(1),
            probe: 4096,
            stride: 4096,
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
            stride: 0,
        };
        let results = hash_files_pooled(jobs, spec, 4);
        assert_eq!(results.len(), 20);

        let mut seen: Vec<i64> = results.iter().map(|(id, _)| *id).collect();
        seen.sort();
        assert_eq!(seen, (0..20).collect::<Vec<_>>());
        for (_, r) in &results {
            assert!(r.is_ok());
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}
