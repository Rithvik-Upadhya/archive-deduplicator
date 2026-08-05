//! Tauri command surface. Every mutating command writes through to SQLite so the
//! full application state is restored on next launch.

use crate::db::Db;
use crate::dedup::DedupParams;
use crate::model::*;
use crate::{hashing, links, medium, parse, pathfix, rollup, scan};
use chrono::Utc;
use rusqlite::params;
use std::collections::HashMap;
use std::path::Path;
use tauri::{Emitter, Manager, State};

type CmdResult<T> = Result<T, String>;

fn now() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn map_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Turns a single-row `query_row` result into `Ok(None)` only for the
/// "no such row" case, surfacing every other error instead of masking it as
/// a plain "not found" the way a blanket `.ok()` would.
fn optional_row<T>(res: rusqlite::Result<T>) -> CmdResult<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(map_err(e)),
    }
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn workspace_list(db: State<Db>) -> CmdResult<Vec<Workspace>> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare("SELECT id, name, created_at, updated_at FROM workspaces ORDER BY id")
        .map_err(map_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Workspace {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })
        .map_err(map_err)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(map_err)
}

#[tauri::command]
pub fn workspace_create(db: State<Db>, name: String) -> CmdResult<Workspace> {
    let conn = db.lock();
    let ts = now();
    conn.execute(
        "INSERT INTO workspaces (name, created_at, updated_at) VALUES (?1, ?2, ?2)",
        params![name, ts],
    )
    .map_err(map_err)?;
    let id = conn.last_insert_rowid();
    Ok(Workspace {
        id,
        name,
        created_at: ts.clone(),
        updated_at: ts,
    })
}

#[tauri::command]
pub fn workspace_rename(db: State<Db>, id: i64, name: String) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE workspaces SET name = ?1, updated_at = ?2 WHERE id = ?3",
        params![name, now(), id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn workspace_delete(db: State<Db>, id: i64) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute("DELETE FROM workspaces WHERE id = ?1", params![id])
        .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sources (devices)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn source_list(db: State<Db>, workspace_id: i64) -> CmdResult<Vec<Source>> {
    let conn = db.lock();
    let dup_by_src = rollup::duplicated_size_by_source(&conn, workspace_id).map_err(map_err)?;
    let cross_dup_by_src =
        rollup::cross_dup_size_by_source(&conn, workspace_id).map_err(map_err)?;
    let hash_phase_by_src = hashing::hash_phase_by_source(&conn, workspace_id).map_err(map_err)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count, excluded, physical_size, alias_bytes, medium_kind, filesystem, hash_min_size, hash_spec, hash_coverage_files, hash_coverage_bytes, hashing_enabled
             FROM sources WHERE workspace_id = ?1 ORDER BY id",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![workspace_id], |r| {
            Ok(Source {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                kind: r.get(2)?,
                label: r.get(3)?,
                device_label: r.get(4)?,
                orig_root_path: r.get(5)?,
                dev_id: r.get(6)?,
                imported_at: r.get(7)?,
                total_size: r.get(8)?,
                file_count: r.get(9)?,
                excluded: r.get::<_, i64>(10)? != 0,
                duplicated_pct: 0.0,
                cross_dup_size: 0,
                cross_dup_file_count: 0,
                physical_size: r.get(11)?,
                alias_bytes: r.get(12)?,
                medium_kind: r.get(13)?,
                filesystem: r.get(14)?,
                hash_min_size: r.get(15)?,
                hash_spec: r.get(16)?,
                hash_coverage_files: r.get(17)?,
                hash_coverage_bytes: r.get(18)?,
                hashing_enabled: r.get::<_, i64>(19)? != 0,
                hashing_phase: None,
            })
        })
        .map_err(map_err)?;
    let mut sources = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    for s in &mut sources {
        let dup = *dup_by_src.get(&s.id).unwrap_or(&0);
        s.duplicated_pct = if s.total_size > 0 {
            dup as f64 / s.total_size as f64 * 100.0
        } else {
            0.0
        };
        let (cross_size, cross_count) = cross_dup_by_src.get(&s.id).copied().unwrap_or((0, 0));
        s.cross_dup_size = cross_size;
        s.cross_dup_file_count = cross_count;
        s.hashing_phase = hash_phase_by_src.get(&s.id).cloned();
    }
    Ok(sources)
}

/// Import a `tree` JSON document as a new source, flattening it into `nodes`.
///
/// Runs on a dedicated blocking thread via `spawn_blocking` rather than
/// inline: Tauri calls non-async commands directly on whatever thread is
/// servicing the IPC message (see `tauri-macros`' `body_blocking`), which on
/// desktop is the webview's main thread -- a synchronous command of any
/// real duration freezes the whole UI, not just the DB. Making the command
/// `async` and doing the actual work inside `spawn_blocking` keeps parsing
/// and the DB write off that thread.
#[tauri::command]
pub async fn import_tree_json(
    app: tauri::AppHandle,
    workspace_id: i64,
    json_text: String,
    label: String,
) -> CmdResult<Source> {
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<Source> {
        let flat = parse::parse_tree_json(&json_text)?;
        let db = app.state::<Db>();
        let mut conn = db.lock();
        let ts = now();
        let device_label = label.clone();
        let tx = conn.transaction().map_err(map_err)?;
        tx.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count, hashing_enabled)
             VALUES (?1, 'json', ?2, ?3, NULL, ?4, ?5, ?6, ?7, 0)",
            params![workspace_id, label, device_label, flat.root_dev, ts, flat.total_size, flat.file_count],
        )
        .map_err(map_err)?;
        let source_id = tx.last_insert_rowid();
        parse::insert_nodes(&tx, source_id, &flat).map_err(map_err)?;
        let (physical_size, alias_bytes) =
            links::collapse_hardlinks_and_recompute(&tx, source_id).map_err(map_err)?;
        tx.commit().map_err(map_err)?;

        Ok(Source {
            id: source_id,
            workspace_id,
            kind: "json".into(),
            label,
            device_label,
            orig_root_path: None,
            dev_id: flat.root_dev,
            imported_at: ts,
            total_size: flat.total_size,
            file_count: flat.file_count,
            excluded: false,
            duplicated_pct: 0.0,
            cross_dup_size: 0,
            cross_dup_file_count: 0,
            // `tree` JSON imports carry no filesystem/medium signal at all --
            // always "Metadata only" and never hashed.
            medium_kind: None,
            filesystem: None,
            hash_min_size: 65536,
            hash_spec: None,
            hash_coverage_files: 0,
            hash_coverage_bytes: 0,
            hashing_enabled: false,
            hashing_phase: None,
            physical_size,
            alias_bytes,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Scan a local folder as a new source (works without the `tree` binary).
/// See `import_tree_json`'s doc comment for why this runs inside
/// `spawn_blocking` -- this one matters even more, since a folder walk on a
/// slow disk is the whole reason progress reporting exists here.
#[tauri::command]
pub async fn scan_folder(
    app: tauri::AppHandle,
    workspace_id: i64,
    path: String,
    label: String,
    hash_min_size: Option<i64>,
    medium_override: Option<String>,
    filesystem_override: Option<String>,
    hashing_enabled: bool,
) -> CmdResult<Source> {
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<Source> {
        let detected = medium::detect(Path::new(&path));
        let medium_kind = medium_override
            .as_deref()
            .and_then(medium::MediumKind::parse)
            .unwrap_or(detected.medium_kind);
        let hash_min_size =
            hash_min_size.unwrap_or_else(|| medium::profile_for(medium_kind).hash_min_size_default);

        let flat = scan::scan_folder(Path::new(&path), &detected, |current| {
            let _ = app.emit("scan:progress", ScanProgress { current });
        })?;
        // The volume serial (Windows) / `st_dev` (Linux, unstable across
        // remounts of removable media) -- kept separate from `nodes.dev`
        // (used only for within-scan hardlink detection) because
        // `hash_cache` needs a key stable enough to trust across sessions.
        let volume_id = detected.volume_id.clone();
        let filesystem = filesystem_override.or(detected.filesystem);
        let db = app.state::<Db>();
        let mut conn = db.lock();
        let ts = now();
        let device_label = label.clone();
        let tx = conn.transaction().map_err(map_err)?;
        tx.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count, medium_kind, filesystem, hash_min_size, volume_id, hashing_enabled)
             VALUES (?1, 'scan', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![workspace_id, label, device_label, path, flat.root_dev, ts, flat.total_size, flat.file_count, medium_kind.as_str(), filesystem, hash_min_size, volume_id, hashing_enabled],
        )
        .map_err(map_err)?;
        let source_id = tx.last_insert_rowid();
        parse::insert_nodes(&tx, source_id, &flat).map_err(map_err)?;
        let (physical_size, alias_bytes) =
            links::collapse_hardlinks_and_recompute(&tx, source_id).map_err(map_err)?;
        tx.commit().map_err(map_err)?;

        Ok(Source {
            id: source_id,
            workspace_id,
            kind: "scan".into(),
            label,
            device_label,
            orig_root_path: Some(path),
            dev_id: flat.root_dev,
            imported_at: ts,
            total_size: flat.total_size,
            file_count: flat.file_count,
            excluded: false,
            duplicated_pct: 0.0,
            medium_kind: Some(medium_kind.as_str().to_string()),
            filesystem,
            hash_min_size,
            hash_spec: None,
            hash_coverage_files: 0,
            hash_coverage_bytes: 0,
            hashing_enabled,
            hashing_phase: None,
            cross_dup_size: 0,
            cross_dup_file_count: 0,
            physical_size,
            alias_bytes,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Detected storage-medium/filesystem info for `path`'s volume, shown in the
/// scan-configuration dialog before the user commits to scanning. Never
/// fails outright -- see `medium::detect`'s own doc comment.
#[tauri::command]
pub async fn detect_medium(path: String) -> CmdResult<MediumInfoDto> {
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<MediumInfoDto> {
        let info = medium::detect(Path::new(&path));
        Ok(MediumInfoDto {
            medium_kind: info.medium_kind.as_str().to_string(),
            filesystem: info.filesystem,
            volume_id: info.volume_id,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// A walk-only dry run (no DB writes, metadata only, zero file opens) so the
/// scan-config dialog's "~X of Y files will be hashed" estimate and time
/// estimate are computed from real numbers rather than guessed.
#[tauri::command]
pub async fn preview_scan(path: String, hash_min_size: i64) -> CmdResult<ScanPreview> {
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<ScanPreview> {
        let mut preview = ScanPreview {
            total_files: 0,
            total_bytes: 0,
            files_above_threshold: 0,
            bytes_above_threshold: 0,
        };
        for entry in walkdir::WalkDir::new(&path)
            .min_depth(1)
            .follow_links(false)
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len() as i64;
            preview.total_files += 1;
            preview.total_bytes += size;
            if size >= hash_min_size {
                preview.files_above_threshold += 1;
                preview.bytes_above_threshold += size;
            }
        }
        Ok(preview)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Split out from `get_size_buckets` so it's testable without a Tauri
/// `AppHandle`, mirroring `fetch_medium_and_hashing_enabled` above.
fn size_buckets_for_source(
    conn: &rusqlite::Connection,
    source_id: i64,
) -> rusqlite::Result<Vec<SizeBucketDto>> {
    let mut stmt = conn.prepare(
        "SELECT
           CASE
             WHEN n.size = 0          THEN '00. empty'
             WHEN n.size <       1024 THEN '01. <1K'
             WHEN n.size <       4096 THEN '02. 1K-4K'
             WHEN n.size <      16384 THEN '03. 4K-16K'
             WHEN n.size <      65536 THEN '04. 16K-64K'
             WHEN n.size <     262144 THEN '05. 64K-256K'
             WHEN n.size <    1048576 THEN '06. 256K-1M'
             WHEN n.size <    4194304 THEN '07. 1M-4M'
             WHEN n.size <    8388608 THEN '08. 4M-8M'
             WHEN n.size <   33554432 THEN '09. 8M-32M'
             WHEN n.size <  134217728 THEN '10. 32M-128M'
             WHEN n.size <  536870912 THEN '11. 128M-512M'
             WHEN n.size < 2147483648 THEN '12. 512M-2G'
             WHEN n.size < 8589934592 THEN '13. 2G-8G'
             ELSE                          '14. >=8G'
           END AS bucket,
           COUNT(*) AS files,
           ROUND(SUM(n.size) / 1073741824.0, 3) AS gib,
           ROUND(100.0 * COUNT(*) / SUM(COUNT(*)) OVER (), 2) AS pct_files,
           ROUND(100.0 * SUM(n.size) / SUM(SUM(n.size)) OVER (), 4) AS pct_bytes
         FROM nodes n
         WHERE n.source_id = ?1 AND n.type = 'file'
         GROUP BY bucket
         ORDER BY bucket",
    )?;
    let rows = stmt.query_map(params![source_id], |r| {
        Ok(SizeBucketDto {
            bucket: r.get(0)?,
            files: r.get(1)?,
            gib: r.get(2)?,
            pct_files: r.get(3)?,
            pct_bytes: r.get(4)?,
        })
    })?;
    rows.collect()
}

/// The post-scan file-size distribution for `source_id`, bucketed for the
/// hash-threshold refinement step (shown after a scan completes, before
/// hashing starts) so "skip hashing below" can be set against this source's
/// real size distribution rather than a blind pre-scan guess. Bucket
/// boundaries are fixed powers-of-two-ish breakpoints, not user-configurable.
#[tauri::command]
pub fn get_size_buckets(db: State<Db>, source_id: i64) -> CmdResult<Vec<SizeBucketDto>> {
    let conn = db.lock();
    size_buckets_for_source(&conn, source_id).map_err(map_err)
}

/// Split out from `preview_hash_threshold` so it's testable without a Tauri
/// `AppHandle`.
fn hash_threshold_preview(
    conn: &rusqlite::Connection,
    source_id: i64,
    hash_min_size: i64,
) -> rusqlite::Result<ScanPreview> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(size), 0),
                COALESCE(SUM(CASE WHEN size >= ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN size >= ?2 THEN size ELSE 0 END), 0)
         FROM nodes WHERE source_id = ?1 AND type = 'file'",
        params![source_id, hash_min_size],
        |r| {
            Ok(ScanPreview {
                total_files: r.get(0)?,
                total_bytes: r.get(1)?,
                files_above_threshold: r.get(2)?,
                bytes_above_threshold: r.get(3)?,
            })
        },
    )
}

/// Live "N files / M bytes would be hashed" counts for `source_id` against a
/// candidate `hash_min_size`, queried straight from already-scanned `nodes`
/// rows (cheap -- no disk I/O, unlike `preview_scan`'s disk walk) so the
/// hash-threshold refinement step can update its counter on every keystroke.
#[tauri::command]
pub fn preview_hash_threshold(
    db: State<Db>,
    source_id: i64,
    hash_min_size: i64,
) -> CmdResult<ScanPreview> {
    let conn = db.lock();
    hash_threshold_preview(&conn, source_id, hash_min_size).map_err(map_err)
}

/// Split out from `set_hash_min_size` so it's testable without a Tauri
/// `AppHandle`.
fn update_hash_min_size(
    conn: &rusqlite::Connection,
    source_id: i64,
    hash_min_size: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE sources SET hash_min_size = ?1 WHERE id = ?2",
        params![hash_min_size, source_id],
    )?;
    Ok(())
}

/// Applies the hash-threshold refinement step's confirmed value to
/// `source_id` before hashing starts -- `run_hash_scan` reads
/// `sources.hash_min_size` fresh on every call, so this UPDATE is all that's
/// needed for the refined value to actually take effect.
#[tauri::command]
pub fn set_hash_min_size(db: State<Db>, source_id: i64, hash_min_size: i64) -> CmdResult<()> {
    let conn = db.lock();
    update_hash_min_size(&conn, source_id, hash_min_size).map_err(map_err)
}

fn to_hash_spec_dto(spec: hashing::HashSpec) -> HashSpecDto {
    HashSpecDto {
        threshold: spec.threshold,
        probe: spec.probe,
        max_probes: spec.max_probes,
        spec_string: spec.spec_string(),
    }
}

/// A workspace's digest-affecting hash settings (§6.1) -- locked read-only
/// in the UI once any source in the workspace has been hashed.
#[tauri::command]
pub fn hash_settings_get(db: State<Db>, workspace_id: i64) -> CmdResult<HashSettings> {
    let conn = db.lock();
    let (spec, locked) = hashing::get_hash_settings(&conn, workspace_id).map_err(map_err)?;
    Ok(HashSettings {
        spec: to_hash_spec_dto(spec),
        locked,
    })
}

/// Change the workspace's hash spec before its first hashed scan. Rejected
/// (as an `Err` the frontend should surface plainly) once locked.
#[tauri::command]
pub fn hash_settings_set(db: State<Db>, workspace_id: i64, spec: HashSpecDto) -> CmdResult<()> {
    let conn = db.lock();
    hashing::set_hash_settings(
        &conn,
        workspace_id,
        hashing::HashSpec {
            threshold: spec.threshold,
            probe: spec.probe,
            max_probes: spec.max_probes,
        },
    )
}

/// Reads the two `sources` columns `run_hash_scan` needs to decide whether
/// (and how) to hash: the detected medium (drives queue depth) and whether
/// the user has opted this source out of hashing entirely. Split out from
/// `run_hash_scan` so the disabled-hashing gate is testable without a Tauri
/// `AppHandle`.
fn fetch_medium_and_hashing_enabled(
    conn: &rusqlite::Connection,
    source_id: i64,
) -> rusqlite::Result<(Option<String>, bool)> {
    conn.query_row(
        "SELECT medium_kind, hashing_enabled FROM sources WHERE id = ?1",
        params![source_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// Hash every eligible file in `source_id` under the workspace's hash spec.
/// Safe to call again after an interruption -- see `hashing::run_hash_scan`'s
/// doc comment for why resumability needs no separate resume path. Follows
/// `run_dedup`'s own-connection pattern since this can run for hours.
#[tauri::command]
pub async fn run_hash_scan(app: tauri::AppHandle, source_id: i64) -> CmdResult<HashScanReportDto> {
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<HashScanReportDto> {
        let db_path = app.state::<crate::db::DbPath>().0.clone();
        let mut conn = crate::db::open(&db_path).map_err(map_err)?;

        let (medium_kind_str, hashing_enabled) =
            fetch_medium_and_hashing_enabled(&conn, source_id).map_err(map_err)?;
        // A source the user explicitly opted out of hashing (per-device toggle
        // in the scan dialog) must never get a content-hash pass, even if this
        // command is invoked defensively or by a future "resume hashing"
        // affordance -- the flag is the source of truth, not just a
        // client-side skip.
        if !hashing_enabled {
            return Ok(HashScanReportDto {
                hashed: 0,
                cached: 0,
                errors: 0,
                cancelled: false,
            });
        }
        let medium_kind = medium_kind_str
            .as_deref()
            .and_then(medium::MediumKind::parse)
            .unwrap_or(medium::MediumKind::Unknown);
        let lanes = hashing::LaneConfig::from_profile(&medium::profile_for(medium_kind));

        // Registered for the duration of this call so `cancel_hash_scan` has
        // something to flip. Removed below regardless of outcome (via
        // `result`, before the `?`) so a stale entry never lingers past this
        // call and makes a future cancel for the same source a silent no-op.
        let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.state::<hashing::HashCancelFlags>()
            .0
            .lock()
            .unwrap()
            .insert(source_id, cancel_flag.clone());

        let result = hashing::run_hash_scan(
            &mut conn,
            source_id,
            lanes,
            &cancel_flag,
            |current, total| {
                let _ = app.emit(
                    "hash:progress",
                    HashProgress {
                        source_id,
                        current,
                        total,
                    },
                );
            },
        );

        app.state::<hashing::HashCancelFlags>()
            .0
            .lock()
            .unwrap()
            .remove(&source_id);

        let report = result.map_err(map_err)?;
        Ok(HashScanReportDto {
            hashed: report.hashed as i64,
            cached: report.cached as i64,
            errors: report.errors.len() as i64,
            cancelled: report.cancelled,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Requests that an in-progress `run_hash_scan` for `source_id` stop at its
/// next batch boundary. Returns `true` if a running scan was found and
/// flagged, `false` if nothing is currently hashing that source (a harmless
/// no-op -- e.g. a stale click, or the pass already finished).
#[tauri::command]
pub fn cancel_hash_scan(flags: State<hashing::HashCancelFlags>, source_id: i64) -> CmdResult<bool> {
    let map = flags.0.lock().unwrap();
    if let Some(flag) = map.get(&source_id) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Current `scan_progress` state for a source, if any -- drives a "Resume
/// hashing" banner when a previous `run_hash_scan` call was interrupted.
#[tauri::command]
pub fn get_scan_progress(db: State<Db>, source_id: i64) -> CmdResult<Option<ScanProgressInfo>> {
    let conn = db.lock();
    let progress = hashing::get_scan_progress(&conn, source_id).map_err(map_err)?;
    Ok(progress.map(|(phase, last_cursor)| ScanProgressInfo { phase, last_cursor }))
}

#[tauri::command]
pub fn source_rename_device(db: State<Db>, source_id: i64, device_label: String) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE sources SET device_label = ?1 WHERE id = ?2",
        params![device_label, source_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn source_set_excluded(db: State<Db>, source_id: i64, excluded: bool) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE sources SET excluded = ?1 WHERE id = ?2",
        params![excluded as i64, source_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn source_copy_to_workspace(
    db: State<Db>,
    source_id: i64,
    target_workspace_id: i64,
) -> CmdResult<()> {
    let mut conn = db.lock();
    crate::dbio::copy_source_to_workspace(&mut conn, source_id, target_workspace_id)
        .map_err(map_err)?;
    Ok(())
}

/// Delete every match_group left with fewer than 2 members -- a "duplicate"
/// that no longer has anything to be a duplicate of. Not scoped to any one
/// workspace/source: cheap globally via `idx_members_group`, and correct
/// to run after *any* deletion that could have cascaded away members
/// (currently just source deletion, but the check is unconditionally safe).
fn sweep_orphaned_match_groups(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM match_groups
         WHERE (SELECT COUNT(*) FROM match_members mm WHERE mm.group_id = match_groups.id) < 2",
        [],
    )?;
    Ok(())
}

#[tauri::command]
pub fn source_delete(db: State<Db>, source_id: i64) -> CmdResult<()> {
    let mut conn = db.lock();
    let tx = conn.transaction().map_err(map_err)?;
    tx.execute("DELETE FROM sources WHERE id = ?1", params![source_id])
        .map_err(map_err)?;
    // Cascading deletes just removed this source's nodes, and with them
    // every match_members row that pointed at one (nodes -> match_members is
    // ON DELETE CASCADE), which is what can produce an orphaned group here.
    // Doing this now rather than waiting for the next dedup rerun (which
    // does the same cleanup, but only for kind='hardlink', and only at the
    // start of a pass) means the UI never shows a broken group in the
    // meantime.
    sweep_orphaned_match_groups(&tx).map_err(map_err)?;
    tx.commit().map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree browsing (lazy)
// ---------------------------------------------------------------------------

/// Fetch the direct children of a node (or the roots of a source when
/// `parent_id` is null). Duplicate annotations come from the precomputed
/// `dup_annot` cache (rebuilt on every dedup run) so this stays fast even on
/// very large workspaces.
#[tauri::command]
pub fn get_tree(
    db: State<Db>,
    workspace_id: i64,
    source_id: i64,
    parent_id: Option<i64>,
) -> CmdResult<Vec<Node>> {
    let _ = workspace_id;
    let conn = db.lock();

    let sql = if parent_id.is_some() {
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.rel_path, n.type, n.size, n.mtime, n.inode, n.dev, n.depth, n.subtree_size, n.subtree_file_count,
                COALESCE(d.has_dup, 0), COALESCE(d.dup_pct, 0), COALESCE(d.cross_dup, 0),
                COALESCE(d.cross_dup_size, 0), COALESCE(d.cross_dup_file_count, 0), COALESCE(d.in_folder_group, 0),
                n.alias_of,
                (n.alias_of IS NOT NULL OR EXISTS (SELECT 1 FROM nodes a WHERE a.alias_of = n.id))
         FROM nodes n LEFT JOIN dup_annot d ON d.node_id = n.id
         WHERE n.source_id = ?1 AND n.parent_id = ?2 ORDER BY n.type = 'file', n.name"
    } else {
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.rel_path, n.type, n.size, n.mtime, n.inode, n.dev, n.depth, n.subtree_size, n.subtree_file_count,
                COALESCE(d.has_dup, 0), COALESCE(d.dup_pct, 0), COALESCE(d.cross_dup, 0),
                COALESCE(d.cross_dup_size, 0), COALESCE(d.cross_dup_file_count, 0), COALESCE(d.in_folder_group, 0),
                n.alias_of,
                (n.alias_of IS NOT NULL OR EXISTS (SELECT 1 FROM nodes a WHERE a.alias_of = n.id))
         FROM nodes n LEFT JOIN dup_annot d ON d.node_id = n.id
         WHERE n.source_id = ?1 AND n.parent_id IS NULL ORDER BY n.type = 'file', n.name"
    };
    let mut stmt = conn.prepare(sql).map_err(map_err)?;

    let mapper = |r: &rusqlite::Row| -> rusqlite::Result<Node> {
        Ok(Node {
            id: r.get(0)?,
            source_id: r.get(1)?,
            parent_id: r.get(2)?,
            name: r.get(3)?,
            rel_path: r.get(4)?,
            node_type: r.get(5)?,
            size: r.get(6)?,
            mtime: r.get(7)?,
            inode: r.get(8)?,
            dev: r.get(9)?,
            depth: r.get(10)?,
            subtree_size: r.get(11)?,
            subtree_file_count: r.get(12)?,
            has_duplicate: r.get::<_, i64>(13)? != 0,
            dup_pct: r.get(14)?,
            cross_dup: r.get::<_, i64>(15)? != 0,
            cross_dup_size: r.get(16)?,
            cross_dup_file_count: r.get(17)?,
            in_folder_group: r.get::<_, i64>(18)? != 0,
            alias_of: r.get(19)?,
            is_hardlink: r.get::<_, i64>(20)? != 0,
        })
    };

    let rows = if let Some(pid) = parent_id {
        stmt.query_map(params![source_id, pid], mapper)
    } else {
        stmt.query_map(params![source_id], mapper)
    }
    .map_err(map_err)?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(map_err)
}

// ---------------------------------------------------------------------------
// Dedup
// ---------------------------------------------------------------------------

/// Run the full duplicate-detection pass with the given tuning parameters.
///
/// Runs inside `spawn_blocking` -- see `import_tree_json`'s doc comment for
/// why a plain (non-async) command isn't enough: Tauri calls it directly on
/// the thread servicing the IPC message, which on desktop is the webview's
/// main thread, so anything but a trivially fast command freezes the whole
/// UI, not just the DB.
///
/// It also opens its own connection rather than locking the shared `Db`
/// mutex: this pass can run for a while, and under WAL mode a dedicated
/// writer connection doesn't block readers on `Db` (get_tree, source_list,
/// etc.) even from other threads, so the rest of the app stays responsive
/// while it runs -- see the
/// `wal_mode_lets_a_reader_proceed_during_an_open_writer_transaction` test
/// in `db.rs` for the property this depends on. Both fixes are needed: the
/// dedicated connection alone doesn't help if the whole command still runs
/// on the main thread.
#[tauri::command]
pub async fn run_dedup(
    app: tauri::AppHandle,
    workspace_id: i64,
    min_size_bytes: i64,
    min_confidence: f64,
) -> CmdResult<usize> {
    let params = DedupParams {
        min_size_bytes,
        min_confidence,
    };
    tauri::async_runtime::spawn_blocking(move || -> CmdResult<usize> {
        let db_path = app.state::<crate::db::DbPath>().0.clone();
        let mut conn = crate::db::open(&db_path).map_err(map_err)?;
        crate::dedup::run_with_progress(&mut conn, workspace_id, params, |phase, current, total| {
            let _ = app.emit(
                "dedup:progress",
                DedupProgress {
                    phase: phase.into(),
                    current,
                    total,
                },
            );
        })
        .map_err(map_err)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Return a page of match groups for a workspace, filtered by confidence,
/// minimum size and kind, sorted by confidence or size. Members are fetched
/// with a single batched query per page so pagination stays cheap even with
/// hundreds of thousands of groups.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn get_groups(
    db: State<Db>,
    workspace_id: i64,
    min_confidence: f64,
    min_size: i64,
    kind: Option<String>,
    sort: Option<String>,
    offset: i64,
    limit: i64,
) -> CmdResult<GroupPage> {
    let conn = db.lock();
    let kind_filter = kind.unwrap_or_default();
    let order = match sort.as_deref() {
        Some("size") => "size DESC, confidence DESC",
        _ => "confidence DESC, size DESC",
    };
    let limit = if limit <= 0 { 100 } else { limit.min(500) };

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM match_groups
             WHERE workspace_id = ?1 AND confidence >= ?2 AND size >= ?3
               AND (?4 = '' OR kind = ?4)",
            params![workspace_id, min_confidence, min_size, kind_filter],
            |r| r.get(0),
        )
        .map_err(map_err)?;

    let sql = format!(
        "SELECT id, workspace_id, kind, confidence, primary_signal, size
         FROM match_groups
         WHERE workspace_id = ?1 AND confidence >= ?2 AND size >= ?3
           AND (?4 = '' OR kind = ?4)
         ORDER BY {order} LIMIT ?5 OFFSET ?6"
    );
    let mut stmt = conn.prepare(&sql).map_err(map_err)?;
    let mut groups = stmt
        .query_map(
            params![
                workspace_id,
                min_confidence,
                min_size,
                kind_filter,
                limit,
                offset
            ],
            |r| {
                Ok(MatchGroup {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    kind: r.get(2)?,
                    confidence: r.get(3)?,
                    primary_signal: r.get(4)?,
                    size: r.get(5)?,
                    members: Vec::new(),
                })
            },
        )
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;

    // Batch-load members for this page of groups.
    if !groups.is_empty() {
        let ids: Vec<String> = groups.iter().map(|g| g.id.to_string()).collect();
        let sql = format!(
            "SELECT mm.group_id, n.id, n.source_id, s.device_label, n.rel_path, n.name, n.size, n.subtree_size, n.mtime
             FROM match_members mm
             JOIN nodes n ON n.id = mm.node_id
             JOIN sources s ON s.id = n.source_id
             WHERE mm.group_id IN ({})",
            ids.join(",")
        );
        let mut mstmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = mstmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    MatchMember {
                        node_id: r.get(1)?,
                        source_id: r.get(2)?,
                        device_label: r.get(3)?,
                        rel_path: r.get(4)?,
                        name: r.get(5)?,
                        size: r.get(6)?,
                        subtree_size: r.get(7)?,
                        mtime: r.get(8)?,
                    },
                ))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(map_err)?;
        let mut by_group: HashMap<i64, Vec<MatchMember>> = HashMap::new();
        for (gid, m) in rows {
            by_group.entry(gid).or_default().push(m);
        }
        for g in &mut groups {
            g.members = by_group.remove(&g.id).unwrap_or_default();
        }
    }

    Ok(GroupPage { total, groups })
}

/// Return the match group (with members) that a given node belongs to, if any.
/// Used by the "locate duplicate" button in the device trees.
#[tauri::command]
pub fn get_group_for_node(db: State<Db>, node_id: i64) -> CmdResult<Option<MatchGroup>> {
    let conn = db.lock();
    let group = optional_row(conn.query_row(
        "SELECT mg.id, mg.workspace_id, mg.kind, mg.confidence, mg.primary_signal, mg.size
             FROM match_members mm JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mm.node_id = ?1 ORDER BY mg.confidence DESC LIMIT 1",
        params![node_id],
        |r| {
            Ok(MatchGroup {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                kind: r.get(2)?,
                confidence: r.get(3)?,
                primary_signal: r.get(4)?,
                size: r.get(5)?,
                members: Vec::new(),
            })
        },
    ))?;
    let Some(mut g) = group else { return Ok(None) };
    let mut mstmt = conn
        .prepare(
            "SELECT n.id, n.source_id, s.device_label, n.rel_path, n.name, n.size, n.subtree_size, n.mtime
             FROM match_members mm
             JOIN nodes n ON n.id = mm.node_id
             JOIN sources s ON s.id = n.source_id
             WHERE mm.group_id = ?1",
        )
        .map_err(map_err)?;
    g.members = mstmt
        .query_map(params![g.id], |r| {
            Ok(MatchMember {
                node_id: r.get(0)?,
                source_id: r.get(1)?,
                device_label: r.get(2)?,
                rel_path: r.get(3)?,
                name: r.get(4)?,
                size: r.get(5)?,
                subtree_size: r.get(6)?,
                mtime: r.get(7)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok(Some(g))
}

// ---------------------------------------------------------------------------
// Consolidation
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn consolidation_get(
    db: State<Db>,
    workspace_id: i64,
) -> CmdResult<(i64, Vec<ConsolidationNode>)> {
    let conn = db.lock();
    // Ensure a single consolidation exists per workspace.
    let id: i64 = {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM consolidations WHERE workspace_id = ?1 LIMIT 1",
                params![workspace_id],
                |r| r.get(0),
            )
            .ok();
        match existing {
            Some(i) => i,
            None => {
                conn.execute(
                    "INSERT INTO consolidations (workspace_id, name) VALUES (?1, 'Consolidated')",
                    params![workspace_id],
                )
                .map_err(map_err)?;
                conn.last_insert_rowid()
            }
        }
    };
    let mut stmt = conn
        .prepare(
            "SELECT cn.id, cn.consolidation_id, cn.parent_id, cn.name, cn.type,
                    cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path
             FROM consolidation_nodes cn
             LEFT JOIN nodes n ON n.id = cn.source_node_id
             LEFT JOIN sources s ON s.id = n.source_id
             WHERE cn.consolidation_id = ?1
             ORDER BY cn.parent_id, cn.sort_order",
        )
        .map_err(map_err)?;
    let rows = stmt
        .query_map(params![id], |r| {
            let node_type: String = r.get(4)?;
            Ok(ConsolidationNode {
                id: r.get(0)?,
                consolidation_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                node_type: node_type.clone(),
                source_node_id: r.get(5)?,
                sort_order: r.get(6)?,
                size: if node_type == "directory" {
                    None
                } else {
                    r.get(7)?
                },
                origin_device: r.get(8)?,
                origin_path: r.get(9)?,
            })
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    Ok((id, rows))
}

#[tauri::command]
pub fn consolidation_add_node(
    db: State<Db>,
    consolidation_id: i64,
    parent_id: Option<i64>,
    name: String,
    node_type: String,
    source_node_id: Option<i64>,
) -> CmdResult<ConsolidationNode> {
    let conn = db.lock();
    let sort_order: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order), 0) + 1 FROM consolidation_nodes
             WHERE consolidation_id = ?1 AND parent_id IS ?2",
            params![consolidation_id, parent_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO consolidation_nodes (consolidation_id, parent_id, name, type, source_node_id, sort_order)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![consolidation_id, parent_id, name, node_type, source_node_id, sort_order],
    )
    .map_err(map_err)?;
    let id = conn.last_insert_rowid();
    conn.query_row(
        "SELECT cn.id, cn.consolidation_id, cn.parent_id, cn.name, cn.type,
                cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path
         FROM consolidation_nodes cn
         LEFT JOIN nodes n ON n.id = cn.source_node_id
         LEFT JOIN sources s ON s.id = n.source_id
         WHERE cn.id = ?1",
        params![id],
        |r| {
            let node_type: String = r.get(4)?;
            Ok(ConsolidationNode {
                id: r.get(0)?,
                consolidation_id: r.get(1)?,
                parent_id: r.get(2)?,
                name: r.get(3)?,
                node_type: node_type.clone(),
                source_node_id: r.get(5)?,
                sort_order: r.get(6)?,
                size: if node_type == "directory" {
                    None
                } else {
                    r.get(7)?
                },
                origin_device: r.get(8)?,
                origin_path: r.get(9)?,
            })
        },
    )
    .map_err(map_err)
}

/// Drag-drop a source *directory* wholesale: materializes its entire subtree
/// as real `consolidation_nodes` rows (root + every descendant), so every
/// file/folder inside becomes individually movable/renamable/deletable.
#[tauri::command]
pub fn consolidation_add_source_subtree(
    db: State<Db>,
    consolidation_id: i64,
    parent_id: Option<i64>,
    source_node_id: i64,
) -> CmdResult<Vec<ConsolidationNode>> {
    let mut conn = db.lock();
    crate::consolidate::materialize_subtree(&mut conn, consolidation_id, parent_id, source_node_id)
        .map_err(map_err)
}

#[tauri::command]
pub fn consolidation_move_node(
    db: State<Db>,
    node_id: i64,
    parent_id: Option<i64>,
    sort_order: i64,
) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE consolidation_nodes SET parent_id = ?1, sort_order = ?2 WHERE id = ?3",
        params![parent_id, sort_order, node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn consolidation_rename_node(db: State<Db>, node_id: i64, name: String) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE consolidation_nodes SET name = ?1 WHERE id = ?2",
        params![name, node_id],
    )
    .map_err(map_err)?;
    // Keep the Path-limits view in sync: it prefers pathfix_state.new_name
    // over consolidation_nodes.name whenever a row exists (e.g. this node was
    // already renamed once from that view). This UPDATE is a no-op when no
    // such row exists yet, which is the common case.
    conn.execute(
        "UPDATE pathfix_state SET new_name = ?1 WHERE kind = 'cons' AND ref_id = ?2",
        params![name, node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

#[tauri::command]
pub fn consolidation_delete_node(db: State<Db>, node_id: i64) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "DELETE FROM consolidation_nodes WHERE id = ?1",
        params![node_id],
    )
    .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Path-limit view
// ---------------------------------------------------------------------------

/// Build the consolidated end-state tree annotated with path-length
/// guidance (step 3: dedup → consolidate → fix path limits). Every node is
/// returned, not just over-limit ones; `over_limit` marks every node on an
/// offending root-to-leaf chain.
#[tauri::command]
pub fn pathfix_tree(db: State<Db>, workspace_id: i64, limit: i64) -> CmdResult<Vec<PathTreeNode>> {
    let conn = db.lock();
    pathfix::build_tree(&conn, workspace_id, limit).map_err(map_err)
}

/// Rename a node in the consolidated end-state tree. Empty name reverts the edit.
#[tauri::command]
pub fn pathfix_rename(
    db: State<Db>,
    workspace_id: i64,
    node_id: i64,
    new_name: String,
) -> CmdResult<()> {
    let conn = db.lock();
    pathfix::rename(&conn, workspace_id, node_id, &new_name).map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// App state (UI persistence)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn app_state_get(db: State<Db>, key: String) -> CmdResult<Option<String>> {
    let conn = db.lock();
    optional_row(conn.query_row(
        "SELECT value FROM app_state WHERE key = ?1",
        params![key],
        |r| r.get(0),
    ))
}

#[tauri::command]
pub fn app_state_set(db: State<Db>, key: String, value: String) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO app_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(map_err)?;
    Ok(())
}

/// Same as `app_state_get`/`app_state_set` but scoped to one workspace --
/// used for settings that shouldn't bleed across workspaces (dedup tuning,
/// staleness), unlike `app_state`'s app-wide keys (active workspace, view).
#[tauri::command]
pub fn workspace_state_get(
    db: State<Db>,
    workspace_id: i64,
    key: String,
) -> CmdResult<Option<String>> {
    let conn = db.lock();
    optional_row(conn.query_row(
        "SELECT value FROM workspace_state WHERE workspace_id = ?1 AND key = ?2",
        params![workspace_id, key],
        |r| r.get(0),
    ))
}

#[tauri::command]
pub fn workspace_state_set(
    db: State<Db>,
    workspace_id: i64,
    key: String,
    value: String,
) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "INSERT INTO workspace_state (workspace_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(workspace_id, key) DO UPDATE SET value = excluded.value",
        params![workspace_id, key, value],
    )
    .map_err(map_err)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Database export / import
// ---------------------------------------------------------------------------

/// Export the whole live database (every workspace) as a single SQLite file
/// at the user-chosen `path`.
#[tauri::command]
pub fn db_export(db: State<Db>, path: String) -> CmdResult<()> {
    let conn = db.lock();
    crate::dbio::export_to(&conn, Path::new(&path)).map_err(map_err)
}

/// Import a previously exported SQLite database file at `path`, merging its
/// workspaces (and everything under them) into the live database as new
/// workspaces. Nothing already present is modified or deleted.
#[tauri::command]
pub fn db_import(db: State<Db>, path: String) -> CmdResult<crate::dbio::ImportSummary> {
    let mut conn = db.lock();
    crate::dbio::import_merge(&mut conn, Path::new(&path)).map_err(map_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn seeded_conn() -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO workspaces (name, created_at, updated_at) VALUES ('w', 't', 't')",
            [],
        )
        .unwrap();
        let ws = conn.last_insert_rowid();
        (conn, ws)
    }

    #[test]
    fn fetch_medium_and_hashing_enabled_reflects_a_disabled_source() {
        let (conn, ws) = seeded_conn();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count, medium_kind, hashing_enabled)
             VALUES (?1, 'scan', 'disc-a', 'disc-a', 't', 0, 0, 'hdd', 0)",
            params![ws],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();

        let (medium_kind, hashing_enabled) =
            fetch_medium_and_hashing_enabled(&conn, source_id).unwrap();
        assert_eq!(medium_kind.as_deref(), Some("hdd"));
        assert!(!hashing_enabled);
    }

    #[test]
    fn fetch_medium_and_hashing_enabled_defaults_to_true() {
        let (conn, ws) = seeded_conn();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'scan', 'disc-a', 'disc-a', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        let source_id = conn.last_insert_rowid();

        let (_, hashing_enabled) = fetch_medium_and_hashing_enabled(&conn, source_id).unwrap();
        assert!(hashing_enabled);
    }

    #[test]
    fn sweep_orphaned_match_groups_drops_groups_left_with_fewer_than_two_members() {
        let (conn, ws) = seeded_conn();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'scan', 'disc-a', 'disc-a', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        let source_a = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'scan', 'disc-b', 'disc-b', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        let source_b = conn.last_insert_rowid();

        let insert_node = |source_id: i64| -> i64 {
            conn.execute(
                "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
                 VALUES (?1, NULL, 'a.txt', 'a.txt', 'file', 10)",
                params![source_id],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let node_a1 = insert_node(source_a);
        let node_a2 = insert_node(source_a);
        let node_b = insert_node(source_b);

        // A cross-source duplicate group spanning source_a and source_b --
        // orphaned once source_b (and node_b) is deleted.
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', 55, 'name', 10)",
            params![ws],
        )
        .unwrap();
        let cross_source_group = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            params![cross_source_group, node_a1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            params![cross_source_group, node_b],
        )
        .unwrap();

        // An internal duplicate group entirely within source_a -- unaffected
        // by source_b's deletion, must survive the sweep.
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, 'file', 55, 'name', 10)",
            params![ws],
        )
        .unwrap();
        let internal_group = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            params![internal_group, node_a1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
            params![internal_group, node_a2],
        )
        .unwrap();

        // Deleting source_b cascades away node_b and its match_members row,
        // leaving `cross_source_group` with just node_a1 -- exactly the
        // state `source_delete` produces in production.
        conn.execute("DELETE FROM sources WHERE id = ?1", params![source_b])
            .unwrap();

        let member_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM match_members WHERE group_id = ?1",
                params![cross_source_group],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(member_count, 1, "cascade should have removed node_b's row");

        sweep_orphaned_match_groups(&conn).unwrap();

        let remaining_group_ids: Vec<i64> = conn
            .prepare("SELECT id FROM match_groups ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            remaining_group_ids,
            vec![internal_group],
            "the orphaned cross-source group must be swept away, the unaffected internal group must survive"
        );
    }

    fn insert_source(conn: &Connection, ws: i64) -> i64 {
        conn.execute(
            "INSERT INTO sources (workspace_id, kind, label, device_label, imported_at, total_size, file_count)
             VALUES (?1, 'scan', 'disc-a', 'disc-a', 't', 0, 0)",
            params![ws],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_file(conn: &Connection, source_id: i64, name: &str, size: i64) {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
             VALUES (?1, NULL, ?2, ?2, 'file', ?3)",
            params![source_id, name, size],
        )
        .unwrap();
    }

    #[test]
    fn size_buckets_for_source_groups_by_boundary_and_computes_percentages() {
        let (conn, ws) = seeded_conn();
        let source_id = insert_source(&conn, ws);
        // Two files in the "<1K" bucket, one in "1K-4K" -- 3 files total,
        // so pct_files should read 66.67 / 33.33 and pct_bytes should split
        // by actual byte share, not file count.
        insert_file(&conn, source_id, "a.txt", 100);
        insert_file(&conn, source_id, "b.txt", 200);
        insert_file(&conn, source_id, "c.txt", 2000);

        let buckets = size_buckets_for_source(&conn, source_id).unwrap();
        assert_eq!(
            buckets.iter().map(|b| b.bucket.as_str()).collect::<Vec<_>>(),
            vec!["01. <1K", "02. 1K-4K"]
        );
        let small = &buckets[0];
        assert_eq!(small.files, 2);
        let medium = &buckets[1];
        assert_eq!(medium.files, 1);
        let total_pct_files: f64 = buckets.iter().map(|b| b.pct_files).sum();
        assert!(
            (total_pct_files - 100.0).abs() < 0.1,
            "pct_files across all buckets should sum to ~100, got {total_pct_files}"
        );
    }

    #[test]
    fn size_buckets_for_source_only_counts_files_not_directories() {
        let (conn, ws) = seeded_conn();
        let source_id = insert_source(&conn, ws);
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
             VALUES (?1, NULL, 'dir', 'dir', 'directory', 0)",
            params![source_id],
        )
        .unwrap();
        insert_file(&conn, source_id, "a.txt", 100);

        let buckets = size_buckets_for_source(&conn, source_id).unwrap();
        let total_files: i64 = buckets.iter().map(|b| b.files).sum();
        assert_eq!(total_files, 1, "the directory row must not be counted");
    }

    #[test]
    fn size_buckets_for_source_is_scoped_to_one_source() {
        let (conn, ws) = seeded_conn();
        let source_a = insert_source(&conn, ws);
        let source_b = insert_source(&conn, ws);
        insert_file(&conn, source_a, "a.txt", 100);
        insert_file(&conn, source_b, "b.txt", 100);
        insert_file(&conn, source_b, "c.txt", 200);

        let buckets_a = size_buckets_for_source(&conn, source_a).unwrap();
        let total_a: i64 = buckets_a.iter().map(|b| b.files).sum();
        assert_eq!(total_a, 1, "source_a's bucket table must not include source_b's files");
    }

    #[test]
    fn hash_threshold_preview_counts_only_files_at_or_above_the_threshold() {
        let (conn, ws) = seeded_conn();
        let source_id = insert_source(&conn, ws);
        insert_file(&conn, source_id, "small.txt", 100);
        insert_file(&conn, source_id, "big.txt", 10_000);

        let preview = hash_threshold_preview(&conn, source_id, 1000).unwrap();
        assert_eq!(preview.total_files, 2);
        assert_eq!(preview.total_bytes, 10_100);
        assert_eq!(preview.files_above_threshold, 1);
        assert_eq!(preview.bytes_above_threshold, 10_000);
    }

    #[test]
    fn hash_threshold_preview_at_zero_counts_every_file() {
        let (conn, ws) = seeded_conn();
        let source_id = insert_source(&conn, ws);
        insert_file(&conn, source_id, "a.txt", 1);
        insert_file(&conn, source_id, "b.txt", 2);

        let preview = hash_threshold_preview(&conn, source_id, 0).unwrap();
        assert_eq!(preview.files_above_threshold, preview.total_files);
        assert_eq!(preview.bytes_above_threshold, preview.total_bytes);
    }

    #[test]
    fn hash_threshold_preview_on_an_empty_source_returns_zeros_not_an_error() {
        let (conn, ws) = seeded_conn();
        let source_id = insert_source(&conn, ws);

        let preview = hash_threshold_preview(&conn, source_id, 1000).unwrap();
        assert_eq!(preview.total_files, 0);
        assert_eq!(preview.total_bytes, 0);
        assert_eq!(preview.files_above_threshold, 0);
        assert_eq!(preview.bytes_above_threshold, 0);
    }

    #[test]
    fn update_hash_min_size_persists_and_only_touches_the_target_source() {
        let (conn, ws) = seeded_conn();
        let source_a = insert_source(&conn, ws);
        let source_b = insert_source(&conn, ws);

        // 131072 is deliberately different from the schema's own
        // DEFAULT 65536 for `hash_min_size`, so source_b keeping its
        // untouched default is a meaningful assertion, not a coincidence.
        update_hash_min_size(&conn, source_a, 131072).unwrap();

        let get_hash_min_size = |source_id: i64| -> i64 {
            conn.query_row(
                "SELECT hash_min_size FROM sources WHERE id = ?1",
                params![source_id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(get_hash_min_size(source_a), 131072);
        assert_eq!(
            get_hash_min_size(source_b),
            65536,
            "updating source_a must not touch source_b's default hash_min_size"
        );
    }
}
