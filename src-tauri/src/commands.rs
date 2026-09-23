//! Tauri command surface. Every mutating command writes through to SQLite so the
//! full application state is restored on next launch.

use crate::db::Db;
use crate::dedup::DedupParams;
use crate::model::*;
use crate::{hashing, links, medium, parse, pathfix, rollup, scan, search};
use chrono::Utc;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter};
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
            "SELECT id, workspace_id, kind, label, device_label, orig_root_path, dev_id, imported_at, total_size, file_count, excluded, physical_size, alias_bytes, medium_kind, filesystem, hash_min_size, hash_spec, hash_coverage_files, hash_coverage_bytes, hashing_enabled, color
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
                cross_duplicated_pct: 0.0,
                cross_dup_size: 0,
                cross_dup_file_count: 0,
                cross_dup_alias_bytes: 0,
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
                color: r.get(20)?,
            })
        })
        .map_err(map_err)?;
    let mut sources = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    for s in &mut sources {
        let dup = dup_by_src.get(&s.id).copied().unwrap_or_default();
        // Physical, not logical: `dup` can only ever range over canonical
        // files, since the matcher drops hardlink aliases from its candidate
        // pool. Dividing by `total_size` (which counts every alias) would cap
        // the badge at `physical_size / total_size` -- on a device where a
        // third of the bytes are aliases, a perfect duplicate reads as ~63%.
        //
        // The fallback is load-bearing, not defensive: `physical_size` was
        // added with `DEFAULT 0` (db.rs) and `migrate` never backfills it, so
        // sources imported before that migration still carry 0 here. Every
        // source created since gets it from
        // `links::collapse_hardlinks_and_recompute`, which writes it on both
        // the trusted and untrusted paths.
        let base = if s.physical_size > 0 {
            s.physical_size
        } else {
            s.total_size
        };
        // Both halves divide by the same `base` and come from the same pass,
        // so `cross_duplicated_pct` is always <= `duplicated_pct` and the
        // remainder is exactly the internal-only share the bar draws in grey.
        let share = |bytes: i64| {
            if base > 0 {
                bytes as f64 / base as f64 * 100.0
            } else {
                0.0
            }
        };
        s.duplicated_pct = share(dup.total());
        s.cross_duplicated_pct = share(dup.cross);
        let cross = cross_dup_by_src.get(&s.id).copied().unwrap_or_default();
        s.cross_dup_size = cross.size;
        s.cross_dup_file_count = cross.file_count;
        s.cross_dup_alias_bytes = cross.alias_bytes;
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
            cross_duplicated_pct: 0.0,
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
            color: None,
            physical_size,
            alias_bytes,
            cross_dup_alias_bytes: 0,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Scan a local folder as a new source (works without the `tree` binary).
/// See `import_tree_json`'s doc comment for why this runs inside
/// `spawn_blocking` -- this one matters even more, since a folder walk on a
/// slow disk is the whole reason progress reporting exists here.
// A `#[tauri::command]`'s arity *is* its IPC signature: these eight are the
// arguments `scan_folder` takes in `src/lib/api.ts`. Folding them into a
// struct to satisfy the lint would be a Rust/TS contract change (model.rs,
// types.ts and api.ts in lockstep) bought purely with style.
#[allow(clippy::too_many_arguments)]
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
            cross_duplicated_pct: 0.0,
            medium_kind: Some(medium_kind.as_str().to_string()),
            filesystem,
            hash_min_size,
            hash_spec: None,
            hash_coverage_files: 0,
            hash_coverage_bytes: 0,
            hashing_enabled,
            hashing_phase: None,
            color: None,
            cross_dup_size: 0,
            cross_dup_file_count: 0,
            cross_dup_alias_bytes: 0,
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
pub fn source_set_color(db: State<Db>, source_id: i64, color: String) -> CmdResult<()> {
    let conn = db.lock();
    conn.execute(
        "UPDATE sources SET color = ?1 WHERE id = ?2",
        params![color, source_id],
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

    // Strictly before the `DELETE FROM sources` below: the cascade into `nodes`
    // nulls out `consolidation_nodes.source_node_id`, which is the only thing
    // tying a consolidation row back to this source. See the doc comment there.
    crate::consolidate::purge_source_files(&tx, source_id).map_err(map_err)?;

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

/// The host OS path separator, so the frontend can render and copy paths the
/// way the user's own file manager writes them. Cheaper and more honest than
/// sniffing the webview's user agent.
#[tauri::command]
pub fn path_separator() -> String {
    std::path::MAIN_SEPARATOR.to_string()
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
                (n.alias_of IS NOT NULL OR EXISTS (SELECT 1 FROM nodes a WHERE a.alias_of = n.id)),
                COALESCE(d.skipped, 0), COALESCE(d.skipped_count, 0)
         FROM nodes n LEFT JOIN dup_annot d ON d.node_id = n.id
         WHERE n.source_id = ?1 AND n.parent_id = ?2 ORDER BY n.type = 'file', n.name"
    } else {
        "SELECT n.id, n.source_id, n.parent_id, n.name, n.rel_path, n.type, n.size, n.mtime, n.inode, n.dev, n.depth, n.subtree_size, n.subtree_file_count,
                COALESCE(d.has_dup, 0), COALESCE(d.dup_pct, 0), COALESCE(d.cross_dup, 0),
                COALESCE(d.cross_dup_size, 0), COALESCE(d.cross_dup_file_count, 0), COALESCE(d.in_folder_group, 0),
                n.alias_of,
                (n.alias_of IS NOT NULL OR EXISTS (SELECT 1 FROM nodes a WHERE a.alias_of = n.id)),
                COALESCE(d.skipped, 0), COALESCE(d.skipped_count, 0)
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
            skipped: r.get::<_, i64>(21)? != 0,
            skipped_count: r.get(22)?,
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
/// minimum size, kinds, tiers and (optionally) a name search, sorted by tier or
/// size in either direction. Members are fetched
/// with a single batched query per page so pagination stays cheap even with
/// hundreds of thousands of groups.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn get_groups(
    db: State<Db>,
    workspace_id: i64,
    min_confidence: f64,
    min_size: i64,
    kinds: Option<Vec<String>>,
    tiers: Option<Vec<ConfidenceRange>>,
    sort: Option<String>,
    offset: i64,
    limit: i64,
    search: Option<String>,
    case_sensitive: bool,
) -> CmdResult<GroupPage> {
    let conn = db.lock();
    group_page(
        &conn,
        workspace_id,
        min_confidence,
        min_size,
        kinds,
        tiers,
        sort,
        offset,
        limit,
        search,
        case_sensitive,
    )
    .map_err(map_err)
}

/// Push a query parameter and return its numbered placeholder (`?N`), so a
/// `WHERE` clause with a variable number of terms can be built up in order.
fn bind(values: &mut Vec<Value>, value: impl Into<Value>) -> String {
    values.push(value.into());
    format!("?{}", values.len())
}

/// Query one page of match groups. Split out from the `get_groups` command so it
/// can be unit-tested without a Tauri `State<Db>`.
///
/// `kinds` and `tiers` are each "any of": a group passes when its kind is one
/// of `kinds` and its confidence falls in one of the `tiers` bands. `None` or
/// empty means no filter.
#[allow(clippy::too_many_arguments)]
fn group_page(
    conn: &rusqlite::Connection,
    workspace_id: i64,
    min_confidence: f64,
    min_size: i64,
    kinds: Option<Vec<String>>,
    tiers: Option<Vec<ConfidenceRange>>,
    sort: Option<String>,
    offset: i64,
    limit: i64,
    search: Option<String>,
    case_sensitive: bool,
) -> rusqlite::Result<GroupPage> {
    // Every order ends in `id`: rows tied on the sort keys otherwise have no
    // fixed order, and LIMIT/OFFSET paging can repeat or skip them at page
    // edges. Tier order uses the raw confidence, so within a tier the stronger
    // folder groups still come first.
    let order = match sort.as_deref() {
        Some("tier-asc") => "confidence ASC, size DESC, id",
        Some("size-desc") => "size DESC, confidence DESC, id",
        Some("size-asc") => "size ASC, confidence DESC, id",
        _ => "confidence DESC, size DESC, id",
    };
    let limit = if limit <= 0 { 100 } else { limit.min(500) };

    // A group is only a real duplicate set while at least two of its members
    // still sit on a live (non-excluded) source. Deleted sources drop out for
    // free -- their `match_members` rows are already cascade-removed -- but an
    // excluded source keeps all its nodes, so folder groups that were rebuilt
    // before the exclusion, and the `kind='hardlink'` groups `links.rs` writes
    // at import and never rebuilds, would otherwise linger here until (or even
    // past) the next dedup run. Only pay for the correlated count when the
    // workspace actually has an excluded source.
    let has_excluded: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sources WHERE workspace_id = ?1 AND excluded = 1)",
        params![workspace_id],
        |r| r.get(0),
    )?;
    let live_member_guard = if has_excluded {
        "AND (SELECT COUNT(*) FROM match_members mm
              JOIN nodes n ON n.id = mm.node_id
              JOIN sources s ON s.id = n.source_id
              WHERE mm.group_id = match_groups.id AND s.excluded = 0) >= 2"
    } else {
        ""
    };

    // The WHERE clause is built term by term with numbered placeholders, and
    // the count and page queries share its parameter list.
    let mut values: Vec<Value> = Vec::new();
    let ws = bind(&mut values, workspace_id);
    let conf = bind(&mut values, min_confidence);
    let size = bind(&mut values, min_size);
    let mut where_sql = format!(
        "workspace_id = {ws} AND confidence >= {conf} AND size >= {size} {live_member_guard}"
    );
    if let Some(kinds) = kinds.filter(|k| !k.is_empty()) {
        let placeholders: Vec<String> = kinds.into_iter().map(|k| bind(&mut values, k)).collect();
        where_sql += &format!(" AND kind IN ({})", placeholders.join(", "));
    }
    if let Some(tiers) = tiers.filter(|t| !t.is_empty()) {
        let bands: Vec<String> = tiers
            .into_iter()
            .map(|t| {
                let lo = bind(&mut values, t.min);
                match t.max {
                    Some(max) => {
                        let hi = bind(&mut values, max);
                        format!("(confidence >= {lo} AND confidence < {hi})")
                    }
                    None => format!("confidence >= {lo}"),
                }
            })
            .collect();
        where_sql += &format!(" AND ({})", bands.join(" OR "));
    }

    // Name search: keep a group when at least one of its *displayed* members
    // matches. Excluded-source members are filtered here as they are in the
    // member query below -- a member the pane hides must not be the reason a
    // group shows up in it. An empty needle means "no search".
    search::register(conn)?;
    let needle = search
        .map(|q| search::fold_needle(&q, case_sensitive))
        .unwrap_or_default();
    if !needle.is_empty() {
        let needle = bind(&mut values, needle);
        let case_sensitive = bind(&mut values, case_sensitive as i64);
        where_sql += &format!(
            " AND EXISTS (SELECT 1 FROM match_members mm
                  JOIN nodes n ON n.id = mm.node_id
                  JOIN sources s ON s.id = n.source_id
                  WHERE mm.group_id = match_groups.id AND s.excluded = 0
                    AND search_match(n.name, {needle}, {case_sensitive}))"
        );
    }

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM match_groups WHERE {where_sql}"),
        params_from_iter(values.iter()),
        |r| r.get(0),
    )?;

    let lim = bind(&mut values, limit);
    let off = bind(&mut values, offset);
    let sql = format!(
        "SELECT id, workspace_id, kind, confidence, primary_signal, size
         FROM match_groups
         WHERE {where_sql}
         ORDER BY {order} LIMIT {lim} OFFSET {off}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut groups = stmt
        .query_map(params_from_iter(values.iter()), |r| {
            Ok(MatchGroup {
                id: r.get(0)?,
                workspace_id: r.get(1)?,
                kind: r.get(2)?,
                confidence: r.get(3)?,
                primary_signal: r.get(4)?,
                size: r.get(5)?,
                members: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Batch-load members for this page of groups.
    if !groups.is_empty() {
        let ids: Vec<String> = groups.iter().map(|g| g.id.to_string()).collect();
        let sql = format!(
            "SELECT mm.group_id, n.id, n.source_id, s.device_label, n.rel_path, n.name, n.size, n.subtree_size, n.mtime
             FROM match_members mm
             JOIN nodes n ON n.id = mm.node_id
             JOIN sources s ON s.id = n.source_id
             WHERE mm.group_id IN ({}) AND s.excluded = 0",
            ids.join(",")
        );
        let mut mstmt = conn.prepare(&sql)?;
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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

/// Name search over the workspace's device trees (see `search.rs`).
#[tauri::command]
pub fn search_nodes(
    db: State<Db>,
    workspace_id: i64,
    query: String,
    case_sensitive: bool,
) -> CmdResult<NodeSearchResult> {
    let conn = db.lock();
    search::search_nodes(&conn, workspace_id, &query, case_sensitive).map_err(map_err)
}

/// Ancestors of a node and whether the device funnel hides it, so the
/// frontend can expand a lazily-loaded device tree down to the node and
/// scroll it into view (the arrow on a match-group member).
#[tauri::command]
pub fn node_location(db: State<Db>, node_id: i64) -> CmdResult<NodeLocation> {
    let conn = db.lock();
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE chain(id, parent_id, depth) AS (
                 SELECT id, parent_id, 0 FROM nodes WHERE id = ?1
                 UNION ALL
                 SELECT n.id, n.parent_id, c.depth + 1
                 FROM nodes n JOIN chain c ON n.id = c.parent_id
             )
             SELECT c.id, COALESCE(d.cross_dup, 0)
             FROM chain c LEFT JOIN dup_annot d ON d.node_id = c.id
             ORDER BY c.depth DESC",
        )
        .map_err(map_err)?;
    let chain = stmt
        .query_map(params![node_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0))
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)?;
    let hidden_by_filter = chain.iter().any(|&(_, cross)| cross);
    let ancestors = chain
        .into_iter()
        .map(|(id, _)| id)
        .filter(|&id| id != node_id)
        .collect();
    Ok(NodeLocation {
        ancestors,
        hidden_by_filter,
    })
}

/// Return the match group (with members) that a given node belongs to, if any.
/// Used by the "locate duplicate" button in the device trees.
#[tauri::command]
pub fn get_group_for_node(db: State<Db>, node_id: i64) -> CmdResult<Option<MatchGroup>> {
    let conn = db.lock();
    let group = optional_row(conn.query_row(
        "SELECT mg.id, mg.workspace_id, mg.kind, mg.confidence, mg.primary_signal, mg.size
             FROM match_members mm JOIN match_groups mg ON mg.id = mm.group_id
             WHERE mm.node_id = ?1
               AND (SELECT COUNT(*) FROM match_members mm2
                    JOIN nodes n ON n.id = mm2.node_id
                    JOIN sources s ON s.id = n.source_id
                    WHERE mm2.group_id = mg.id AND s.excluded = 0) >= 2
             ORDER BY mg.confidence DESC LIMIT 1",
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
             WHERE mm.group_id = ?1 AND s.excluded = 0",
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
                    cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path,
                    n.alias_of IS NOT NULL, s.id, cn.done, cn.struck, ps.original_name
             FROM consolidation_nodes cn
             LEFT JOIN nodes n ON n.id = cn.source_node_id
             LEFT JOIN sources s ON s.id = n.source_id
             LEFT JOIN consolidations c ON c.id = cn.consolidation_id
             LEFT JOIN pathfix_state ps ON ps.workspace_id = c.workspace_id
                  AND ps.kind = 'cons' AND ps.ref_id = cn.id AND ps.new_name != ''
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
                is_alias: r.get::<_, Option<bool>>(10)?.unwrap_or(false),
                origin_source_id: r.get(11)?,
                done: r.get(12)?,
                struck: r.get(13)?,
                original_name: r.get(14)?,
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
                cn.source_node_id, cn.sort_order, n.size, s.device_label, n.rel_path,
                    n.alias_of IS NOT NULL, s.id, cn.done, cn.struck, ps.original_name
         FROM consolidation_nodes cn
         LEFT JOIN nodes n ON n.id = cn.source_node_id
         LEFT JOIN sources s ON s.id = n.source_id
         LEFT JOIN consolidations c ON c.id = cn.consolidation_id
         LEFT JOIN pathfix_state ps ON ps.workspace_id = c.workspace_id
              AND ps.kind = 'cons' AND ps.ref_id = cn.id AND ps.new_name != ''
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
                is_alias: r.get::<_, Option<bool>>(10)?.unwrap_or(false),
                origin_source_id: r.get(11)?,
                done: r.get(12)?,
                struck: r.get(13)?,
                original_name: r.get(14)?,
            })
        },
    )
    .map_err(map_err)
}

/// Drag-drop a source *directory* wholesale: materializes its entire subtree
/// as real `consolidation_nodes` rows (root + every descendant), so every
/// file/folder inside becomes individually movable/renamable/deletable.
/// With `filter_cross_dup`, descendants hidden by the source's "exclusive to
/// this device" funnel (`dup_annot.cross_dup`) are left out, matching what the
/// user saw when they dragged.
#[tauri::command]
pub fn consolidation_add_source_subtree(
    db: State<Db>,
    consolidation_id: i64,
    parent_id: Option<i64>,
    source_node_id: i64,
    filter_cross_dup: bool,
) -> CmdResult<Vec<ConsolidationNode>> {
    let mut conn = db.lock();
    crate::consolidate::materialize_subtree(
        &mut conn,
        consolidation_id,
        parent_id,
        source_node_id,
        filter_cross_dup,
    )
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

/// Remove nodes (and, via `ON DELETE CASCADE`, everything beneath them) from
/// the consolidated tree. Takes the selection's roots, so no id is reached by
/// two routes.
#[tauri::command]
pub fn consolidation_delete_nodes(db: State<Db>, node_ids: Vec<i64>) -> CmdResult<()> {
    let mut conn = db.lock();
    let tx = conn.transaction().map_err(map_err)?;
    {
        let mut stmt = tx
            .prepare("DELETE FROM consolidation_nodes WHERE id = ?1")
            .map_err(map_err)?;
        for id in node_ids {
            stmt.execute(params![id]).map_err(map_err)?;
        }
    }
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// Set or clear the archivist's "done" mark on each of `node_ids`. Rows only
/// -- `done` is not inherited, so the caller passes every row it means.
#[tauri::command]
pub fn consolidation_set_done(db: State<Db>, node_ids: Vec<i64>, value: bool) -> CmdResult<()> {
    let mut conn = db.lock();
    let tx = conn.transaction().map_err(map_err)?;
    {
        let mut stmt = tx
            .prepare("UPDATE consolidation_nodes SET done = ?1 WHERE id = ?2")
            .map_err(map_err)?;
        for id in node_ids {
            stmt.execute(params![value, id]).map_err(map_err)?;
        }
    }
    tx.commit().map_err(map_err)?;
    Ok(())
}

/// Set or clear the "to delete" mark on each of `node_ids`. This writes the
/// rows' own flag; descendants inherit it at read time.
#[tauri::command]
pub fn consolidation_set_struck(db: State<Db>, node_ids: Vec<i64>, value: bool) -> CmdResult<()> {
    let mut conn = db.lock();
    let tx = conn.transaction().map_err(map_err)?;
    {
        let mut stmt = tx
            .prepare("UPDATE consolidation_nodes SET struck = ?1 WHERE id = ?2")
            .map_err(map_err)?;
        for id in node_ids {
            stmt.execute(params![value, id]).map_err(map_err)?;
        }
    }
    tx.commit().map_err(map_err)?;
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
) -> CmdResult<RenameResult> {
    let conn = db.lock();
    pathfix::rename(&conn, workspace_id, node_id, &new_name).map_err(map_err)
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
            buckets
                .iter()
                .map(|b| b.bucket.as_str())
                .collect::<Vec<_>>(),
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
        assert_eq!(
            total_a, 1,
            "source_a's bucket table must not include source_b's files"
        );
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

    fn insert_node_returning(conn: &Connection, source_id: i64, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO nodes (source_id, parent_id, name, rel_path, type, size)
             VALUES (?1, NULL, ?2, ?2, 'file', 10)",
            params![source_id, name],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn insert_group(conn: &Connection, ws: i64, kind: &str, members: &[i64]) -> i64 {
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, ?2, 100, 'test', 10)",
            params![ws, kind],
        )
        .unwrap();
        let gid = conn.last_insert_rowid();
        for nid in members {
            conn.execute(
                "INSERT INTO match_members (group_id, node_id, role) VALUES (?1, ?2, 'member')",
                params![gid, nid],
            )
            .unwrap();
        }
        gid
    }

    fn set_excluded(conn: &Connection, source_id: i64, excluded: bool) {
        conn.execute(
            "UPDATE sources SET excluded = ?1 WHERE id = ?2",
            params![excluded as i64, source_id],
        )
        .unwrap();
    }

    fn page_all(conn: &Connection, ws: i64) -> GroupPage {
        group_page(conn, ws, 0.0, 0, None, None, None, 0, 100, None, false).unwrap()
    }

    fn page_search(conn: &Connection, ws: i64, q: &str) -> GroupPage {
        group_page(
            conn,
            ws,
            0.0,
            0,
            None,
            None,
            None,
            0,
            100,
            Some(q.into()),
            false,
        )
        .unwrap()
    }

    fn page_with(
        conn: &Connection,
        ws: i64,
        kinds: Option<&[&str]>,
        tiers: Option<Vec<ConfidenceRange>>,
        sort: Option<&str>,
    ) -> GroupPage {
        group_page(
            conn,
            ws,
            0.0,
            0,
            kinds.map(|k| k.iter().map(|s| s.to_string()).collect()),
            tiers,
            sort.map(String::from),
            0,
            100,
            None,
            false,
        )
        .unwrap()
    }

    /// A member-less group: the list query never needs members unless a
    /// source is excluded.
    fn insert_group_at(conn: &Connection, ws: i64, kind: &str, confidence: f64, size: i64) -> i64 {
        conn.execute(
            "INSERT INTO match_groups (workspace_id, kind, confidence, primary_signal, size)
             VALUES (?1, ?2, ?3, 'test', ?4)",
            params![ws, kind, confidence, size],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn group_ids(page: &GroupPage) -> Vec<i64> {
        page.groups.iter().map(|g| g.id).collect()
    }

    #[test]
    fn get_groups_kinds_filter_keeps_any_of_the_listed_kinds() {
        let (conn, ws) = seeded_conn();
        let file = insert_group_at(&conn, ws, "file", 100.0, 10);
        insert_group_at(&conn, ws, "folder", 100.0, 10);
        let hardlink = insert_group_at(&conn, ws, "hardlink", 100.0, 10);

        let page = page_with(&conn, ws, Some(&["file", "hardlink"]), None, None);
        assert_eq!(page.total, 2, "the count must apply the same filter");
        assert_eq!(group_ids(&page), vec![file, hardlink]);
    }

    #[test]
    fn get_groups_tiers_filter_keeps_any_of_the_listed_bands() {
        // Tiers A and D, not adjacent. 99 is B (below A's floor), 94.5 is a
        // folder's weighted score that reads C, and 60 is D.
        let (conn, ws) = seeded_conn();
        let mut id_of = HashMap::new();
        for c in [100.0, 99.0, 94.5, 70.0, 60.0, 55.0, 45.0] {
            id_of.insert(c.to_string(), insert_group_at(&conn, ws, "file", c, 10));
        }
        let tiers = vec![
            ConfidenceRange {
                min: 100.0,
                max: None,
            },
            ConfidenceRange {
                min: 55.0,
                max: Some(70.0),
            },
        ];

        let page = page_with(&conn, ws, None, Some(tiers), None);
        assert_eq!(page.total, 3);
        assert_eq!(
            group_ids(&page),
            vec![id_of["100"], id_of["60"], id_of["55"]]
        );
    }

    #[test]
    fn get_groups_sorts_by_tier_or_size_in_either_direction() {
        let (conn, ws) = seeded_conn();
        let a = insert_group_at(&conn, ws, "file", 100.0, 10);
        let d = insert_group_at(&conn, ws, "file", 55.0, 30);
        let c_small = insert_group_at(&conn, ws, "file", 70.0, 20);
        let c_big = insert_group_at(&conn, ws, "file", 70.0, 40);
        // Tied on every sort key: only the trailing `id` orders these, which
        // is what keeps LIMIT/OFFSET paging from repeating or skipping them.
        let e1 = insert_group_at(&conn, ws, "file", 45.0, 5);
        let e2 = insert_group_at(&conn, ws, "file", 45.0, 5);

        let order = |sort: Option<&str>| group_ids(&page_with(&conn, ws, None, None, sort));
        assert_eq!(
            order(None),
            vec![a, c_big, c_small, d, e1, e2],
            "tier-desc is the default"
        );
        assert_eq!(order(Some("tier-asc")), vec![e1, e2, d, c_big, c_small, a]);
        assert_eq!(order(Some("size-desc")), vec![c_big, d, c_small, a, e1, e2]);
        assert_eq!(order(Some("size-asc")), vec![e1, e2, a, c_small, d, c_big]);
    }

    #[test]
    fn get_groups_search_keeps_a_group_when_one_member_matches() {
        let (conn, ws) = seeded_conn();
        let src_x = insert_source(&conn, ws);
        let src_y = insert_source(&conn, ws);
        let a = insert_node_returning(&conn, src_x, "Holiday.MOV");
        let b = insert_node_returning(&conn, src_y, "clip-0001.mov");
        let group = insert_group(&conn, ws, "file", &[a, b]);
        let c = insert_node_returning(&conn, src_x, "notes.txt");
        let d = insert_node_returning(&conn, src_y, "notes.txt");
        insert_group(&conn, ws, "file", &[c, d]);

        let page = page_search(&conn, ws, "holiday");
        assert_eq!(page.total, 1);
        assert_eq!(page.groups[0].id, group);
        assert_eq!(
            page.groups[0].members.len(),
            2,
            "every member is still listed"
        );
        assert_eq!(
            page_search(&conn, ws, "").total,
            2,
            "empty query is no search"
        );
    }

    #[test]
    fn get_groups_search_ignores_a_match_on_an_excluded_member() {
        let (conn, ws) = seeded_conn();
        let src_x = insert_source(&conn, ws);
        let src_y = insert_source(&conn, ws);
        let src_z = insert_source(&conn, ws);
        let x1 = insert_node_returning(&conn, src_x, "renamed-copy.jpg");
        let y1 = insert_node_returning(&conn, src_y, "img.jpg");
        let z1 = insert_node_returning(&conn, src_z, "img.jpg");
        insert_group(&conn, ws, "file", &[x1, y1, z1]);

        assert_eq!(page_search(&conn, ws, "renamed").total, 1);
        set_excluded(&conn, src_x, true);
        assert_eq!(
            page_search(&conn, ws, "renamed").total,
            0,
            "the only matching member is hidden, so the group must be too"
        );
    }

    #[test]
    fn get_groups_drops_a_group_once_it_lacks_two_live_members() {
        let (conn, ws) = seeded_conn();
        let src_x = insert_source(&conn, ws);
        let src_y = insert_source(&conn, ws);
        let src_z = insert_source(&conn, ws);

        // A hardlink group entirely on src_x (the shape links.rs writes and a
        // dedup re-run never rebuilds) ...
        let x1 = insert_node_returning(&conn, src_x, "x1");
        let x2 = insert_node_returning(&conn, src_x, "x2");
        let hardlink_group = insert_group(&conn, ws, "hardlink", &[x1, x2]);
        // ... and an unrelated cross-source file group on src_y + src_z.
        let y1 = insert_node_returning(&conn, src_y, "shared");
        let z1 = insert_node_returning(&conn, src_z, "shared");
        let file_group = insert_group(&conn, ws, "file", &[y1, z1]);

        // Nothing excluded: both groups visible.
        let page = page_all(&conn, ws);
        assert_eq!(page.total, 2);
        let mut ids: Vec<i64> = page.groups.iter().map(|g| g.id).collect();
        ids.sort();
        assert_eq!(ids, vec![hardlink_group, file_group]);

        // Excluding src_x leaves the hardlink group with 0 live members -- it
        // must disappear from the pane (count included), the file group stays.
        set_excluded(&conn, src_x, true);
        let page = page_all(&conn, ws);
        assert_eq!(page.total, 1);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.groups[0].id, file_group);
        assert_eq!(page.groups[0].members.len(), 2);

        // Re-including src_x brings the hardlink group back untouched.
        set_excluded(&conn, src_x, false);
        assert_eq!(page_all(&conn, ws).total, 2);
    }

    #[test]
    fn get_groups_keeps_a_mixed_group_but_hides_its_excluded_member() {
        let (conn, ws) = seeded_conn();
        let src_x = insert_source(&conn, ws);
        let src_y = insert_source(&conn, ws);
        let src_z = insert_source(&conn, ws);
        let x1 = insert_node_returning(&conn, src_x, "shared");
        let y1 = insert_node_returning(&conn, src_y, "shared");
        let z1 = insert_node_returning(&conn, src_z, "shared");
        let group = insert_group(&conn, ws, "file", &[x1, y1, z1]);

        set_excluded(&conn, src_x, true);
        let page = page_all(&conn, ws);
        assert_eq!(page.total, 1, "2 live members still make a real group");
        assert_eq!(page.groups[0].id, group);
        let member_nodes: Vec<i64> = page.groups[0].members.iter().map(|m| m.node_id).collect();
        assert_eq!(member_nodes.len(), 2);
        assert!(
            !member_nodes.contains(&x1),
            "excluded-source member is hidden"
        );
    }
}
