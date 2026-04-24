use crate::checkpoint;
use crate::config;
use crate::history;
use crate::object_store::sha256_hex;
use crate::path_safety::validate_snapshot_paths;
use crate::repo::{self, RepoStatus};
use crate::snapshot::{
    compute_snapshot_id_for_manifest, load_snapshot, snapshot_path, SnapshotManifest,
};
use crate::trace::TraceStats;
use crate::transaction;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Default)]
pub struct IntegrityReport {
    pub errors: Vec<IntegrityIssue>,
    pub warnings: Vec<IntegrityIssue>,
    pub stats: StorageStats,
}

#[derive(Debug, Clone, Default)]
pub struct StorageStats {
    pub event_count: usize,
    pub event_counts_by_kind: BTreeMap<String, usize>,
    pub checkpoint_count: usize,
    pub reachable_snapshots: BTreeSet<String>,
    pub unreferenced_snapshots: BTreeSet<String>,
    pub reachable_objects: BTreeSet<String>,
    pub unreferenced_objects: BTreeSet<String>,
    pub reachable_object_bytes: u64,
    pub unreferenced_object_bytes: u64,
    pub head_snapshot: String,
    pub active_journal: bool,
    pub trace_stats: TraceStats,
    pub repo_format_version: Option<u32>,
    pub db_schema_version: Option<u32>,
    pub migration_status: String,
    pub config_status: String,
    pub ignore_enabled: bool,
    pub ignore_rule_count: usize,
    pub manifest_versions: BTreeMap<u32, usize>,
    pub symlink_entries: usize,
    pub executable_file_entries: usize,
}

#[derive(Debug, Clone)]
pub struct IntegrityIssue {
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct GcPlan {
    pub snapshots: Vec<String>,
    pub objects: Vec<ObjectRemoval>,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ObjectRemoval {
    pub hash: String,
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone)]
struct ObjectFile {
    path: PathBuf,
    size: u64,
}

pub fn verify(project_dir: &Path) -> Result<IntegrityReport> {
    build_report(project_dir)
}

pub fn stats(project_dir: &Path) -> Result<StorageStats> {
    Ok(build_report(project_dir)?.stats)
}

pub fn gc_plan(project_dir: &Path) -> Result<(IntegrityReport, GcPlan)> {
    let report = build_report(project_dir)?;
    let object_files = list_object_files(project_dir)?;
    let objects = report
        .stats
        .unreferenced_objects
        .iter()
        .filter_map(|hash| {
            object_files.get(hash).map(|object| ObjectRemoval {
                hash: hash.clone(),
                path: object.path.clone(),
                size: object.size,
            })
        })
        .collect::<Vec<_>>();
    let reclaimable_bytes = objects.iter().map(|object| object.size).sum();
    let snapshots = report
        .stats
        .unreferenced_snapshots
        .iter()
        .cloned()
        .collect::<Vec<_>>();

    Ok((
        report,
        GcPlan {
            snapshots,
            objects,
            reclaimable_bytes,
        },
    ))
}

pub fn apply_gc(project_dir: &Path, plan: &GcPlan) -> Result<()> {
    for snapshot in &plan.snapshots {
        let path = snapshot_path(project_dir, snapshot);
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }

    for object in &plan.objects {
        if object.path.exists() {
            fs::remove_file(&object.path)
                .with_context(|| format!("removing {}", object.path.display()))?;
        }
    }

    remove_empty_object_dirs(&project_dir.join(REWIND_DIR).join("objects"))?;
    Ok(())
}

pub fn resolve_snapshot_prefix(project_dir: &Path, prefix: &str) -> Result<String> {
    if prefix.is_empty() {
        bail!("snapshot prefix must not be empty");
    }

    let ids = list_snapshot_ids(project_dir)?;
    let matches = ids
        .into_iter()
        .filter(|id| id.starts_with(prefix))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => bail!("No snapshot matches prefix {prefix}"),
        [id] => Ok(id.clone()),
        _ => {
            let mut message = format!("Ambiguous snapshot prefix {prefix}.\n\nMatches:");
            for id in matches {
                message.push_str(&format!("\n  {id}"));
            }
            message.push_str("\n\nPlease provide a longer prefix.");
            bail!("{message}");
        }
    }
}

fn build_report(project_dir: &Path) -> Result<IntegrityReport> {
    let mut report = IntegrityReport::default();
    let repo_status = repo::inspect(project_dir);
    report.stats.migration_status = repo_status.status.as_str().to_owned();
    report.stats.active_journal = repo_status.active_journal;
    if let Some(manifest) = &repo_status.manifest {
        report.stats.repo_format_version = Some(manifest.format_version);
        report.stats.db_schema_version = Some(manifest.db_schema_version);
        if let Err(error) = repo::validate_manifest_shape(manifest) {
            report.error(format!("Invalid repo manifest: {error}"));
        }
    }
    if let Some(db_schema_version) = repo_status.db_schema_version {
        if report.stats.db_schema_version.is_none() {
            report.stats.db_schema_version = Some(db_schema_version);
        }
    }
    match config::status(project_dir) {
        Ok(status) => {
            report.stats.ignore_enabled = status.config.ignore.enabled;
            report.stats.ignore_rule_count = status.ignore_rule_count;
            report.stats.config_status = if status.config.ignore.enabled {
                if status.ignore_file_exists {
                    "valid".to_owned()
                } else {
                    "valid (defaults/no ignore file)".to_owned()
                }
            } else {
                "valid (ignore disabled)".to_owned()
            };
        }
        Err(error) => {
            report.stats.config_status = "invalid".to_owned();
            report.error(format!("Invalid config/ignore rules: {error:#}"));
        }
    }

    match repo_status.status {
        RepoStatus::Current => {}
        RepoStatus::Uninitialized => {
            bail!(
                "{} is not initialized; run `rewind init` first",
                project_dir.display()
            );
        }
        RepoStatus::NeedsMigration => {
            report.error(format!(
                "Repo needs migration: {}. Run: rewind migrate",
                repo_status
                    .reason
                    .unwrap_or_else(|| "legacy repo format".to_owned())
            ));
            return Ok(report);
        }
        RepoStatus::IncompatibleFutureFormat => {
            report.error(format!(
                "Repo uses a newer unsupported format: {}",
                repo_status
                    .reason
                    .unwrap_or_else(|| "future format".to_owned())
            ));
            return Ok(report);
        }
        RepoStatus::Invalid => {
            report.error(format!(
                "Repo format metadata is invalid: {}",
                repo_status
                    .reason
                    .unwrap_or_else(|| "unknown error".to_owned())
            ));
            return Ok(report);
        }
    }

    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?;
    let checkpoints = read_checkpoints_raw(&conn)?;
    let trace_ids = read_trace_ids(&conn)?;
    let snapshot_ids = list_snapshot_ids(project_dir)?;
    let object_files = list_object_files(project_dir)?;

    report.stats.active_journal = transaction::has_active(project_dir);
    if report.stats.active_journal {
        match transaction::recovery_status(project_dir) {
            Ok(transaction::RecoveryStatus::Active(journal)) => {
                report.warning(format!(
                    "Active journal: {} ({}, phase {:?})",
                    journal.id, journal.operation, journal.phase
                ));
            }
            Ok(transaction::RecoveryStatus::NoActiveTransaction) => {}
            Err(error) => report.error(format!("Invalid active journal: {error:#}")),
        }
    }
    report.stats.event_count = events.len();
    report.stats.checkpoint_count = checkpoints.len();
    report.stats.trace_stats = crate::trace::trace_stats(&conn)?;

    for event in &events {
        *report
            .stats
            .event_counts_by_kind
            .entry(event.kind.clone())
            .or_insert(0) += 1;
        if event.kind.is_empty() {
            report.error(format!("Event {} has empty kind", event.id));
        }
        if let Some(argv_json) = &event.command_argv_json {
            match serde_json::from_str::<serde_json::Value>(argv_json) {
                Ok(serde_json::Value::Array(values))
                    if values.iter().all(|value| value.is_string()) => {}
                Ok(_) => report.error(format!(
                    "Event {} command_argv_json must be a JSON array of strings",
                    event.id
                )),
                Err(error) => report.error(format!(
                    "Event {} command_argv_json is invalid JSON: {error}",
                    event.id
                )),
            }
        }
        if let Err(error) = crate::replay::validate_replay_cwd(&event.command_cwd_relative) {
            report.error(format!(
                "Event {} command_cwd_relative is invalid: {error}",
                event.id
            ));
        }
    }

    verify_trace_metadata(project_dir, &conn, &events, &trace_ids, &mut report)?;

    match history::get_head_snapshot(&conn)? {
        Some(head) => {
            report.stats.head_snapshot = head.clone();
            report.stats.reachable_snapshots.insert(head);
        }
        None => report.error("workspace_state.head_snapshot is missing"),
    }

    for event in &events {
        report
            .stats
            .reachable_snapshots
            .insert(event.before_snapshot.clone());
        report
            .stats
            .reachable_snapshots
            .insert(event.after_snapshot.clone());
    }

    for checkpoint in &checkpoints {
        if let Err(error) = checkpoint::validate_checkpoint_name(&checkpoint.name) {
            report.error(format!(
                "Checkpoint {} has invalid name: {error}",
                checkpoint.name
            ));
        }
        report
            .stats
            .reachable_snapshots
            .insert(checkpoint.snapshot_id.clone());
    }

    for checkpoint in &checkpoints {
        if !snapshot_ids.contains(&checkpoint.snapshot_id) {
            report.error(format!(
                "Checkpoint {} points to missing snapshot {}",
                checkpoint.name, checkpoint.snapshot_id
            ));
        }
    }

    let reachable_snapshots = report
        .stats
        .reachable_snapshots
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    for snapshot_id in reachable_snapshots {
        verify_reachable_snapshot(project_dir, &snapshot_id, &mut report);
    }

    for snapshot_id in &snapshot_ids {
        if !report.stats.reachable_snapshots.contains(snapshot_id) {
            report
                .stats
                .unreferenced_snapshots
                .insert(snapshot_id.clone());
            report.warning(format!("Unreferenced snapshot: {snapshot_id}"));
            if let Ok(snapshot) = load_snapshot(project_dir, snapshot_id) {
                for entry in snapshot.files.values() {
                    if !report.stats.reachable_objects.contains(&entry.hash) {
                        report.stats.unreferenced_objects.insert(entry.hash.clone());
                    }
                }
            }
        }
    }

    for (hash, object) in &object_files {
        if !report.stats.reachable_objects.contains(hash) {
            report.stats.unreferenced_objects.insert(hash.clone());
            report.stats.unreferenced_object_bytes += object.size;
            report.warning(format!(
                "Unreferenced object: {}",
                object
                    .path
                    .strip_prefix(project_dir)
                    .unwrap_or(&object.path)
                    .display()
            ));
        }
    }

    Ok(report)
}

fn verify_trace_metadata(
    project_dir: &Path,
    conn: &rusqlite::Connection,
    events: &[history::Event],
    trace_ids: &BTreeSet<i64>,
    report: &mut IntegrityReport,
) -> Result<()> {
    let event_ids = events.iter().map(|event| event.id).collect::<BTreeSet<_>>();
    let mut stmt = conn.prepare(
        "SELECT id, event_id, raw_trace_path, status
         FROM command_traces
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (trace_id, event_id, raw_trace_path, status) = row?;
        if !event_ids.contains(&event_id) {
            report.error(format!(
                "Trace {trace_id} references missing event {event_id}"
            ));
        }
        if status.is_empty() {
            report.error(format!("Trace {trace_id} has empty status"));
        }
        if let Some(raw_trace_path) = raw_trace_path {
            if !is_safe_raw_trace_path(&raw_trace_path) {
                report.error(format!(
                    "Trace {trace_id} raw path is outside .rewind/traces: {raw_trace_path}"
                ));
            } else if !project_dir.join(&raw_trace_path).exists() {
                report.error(format!(
                    "Trace {trace_id} raw trace file is missing: {raw_trace_path}"
                ));
            }
        }
    }

    verify_trace_child_table(conn, "trace_file_events", trace_ids, report)?;
    verify_trace_child_table(conn, "trace_processes", trace_ids, report)?;
    verify_trace_access_kinds(conn, report)?;
    Ok(())
}

fn is_safe_raw_trace_path(path: &str) -> bool {
    let path = Path::new(path);
    if path.is_absolute() {
        return false;
    }
    let components = path.components().collect::<Vec<_>>();
    if components.len() < 3 {
        return false;
    }
    if components[0] != Component::Normal(REWIND_DIR.as_ref())
        || components[1] != Component::Normal("traces".as_ref())
    {
        return false;
    }
    components
        .iter()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn verify_trace_child_table(
    conn: &rusqlite::Connection,
    table: &str,
    trace_ids: &BTreeSet<i64>,
    report: &mut IntegrityReport,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT id, trace_id FROM {table} ORDER BY id ASC"))?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    for row in rows {
        let (id, trace_id) = row?;
        if !trace_ids.contains(&trace_id) {
            report.error(format!(
                "{table} row {id} references missing trace {trace_id}"
            ));
        }
    }
    Ok(())
}

fn verify_trace_access_kinds(
    conn: &rusqlite::Connection,
    report: &mut IntegrityReport,
) -> Result<()> {
    let mut stmt = conn.prepare("SELECT id, access_kind FROM trace_file_events ORDER BY id ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, access_kind) = row?;
        if !crate::trace::valid_access_kind(&access_kind) {
            report.error(format!(
                "trace_file_events row {id} has invalid access_kind {access_kind}"
            ));
        }
    }
    Ok(())
}

fn verify_reachable_snapshot(project_dir: &Path, snapshot_id: &str, report: &mut IntegrityReport) {
    let path = snapshot_path(project_dir, snapshot_id);
    if !path.exists() {
        report.error(format!("missing snapshot {snapshot_id}"));
        return;
    }

    let snapshot = match load_snapshot(project_dir, snapshot_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            report.error(format!("Snapshot {snapshot_id} is invalid: {error}"));
            return;
        }
    };

    verify_snapshot_manifest(snapshot_id, &snapshot, report);
    *report
        .stats
        .manifest_versions
        .entry(snapshot.manifest_version)
        .or_insert(0) += 1;
    report.stats.symlink_entries += snapshot.symlinks.len();
    report.stats.executable_file_entries += snapshot
        .files
        .values()
        .filter(|entry| entry.executable)
        .count();

    for (file_path, entry) in &snapshot.files {
        let first_reference = report.stats.reachable_objects.insert(entry.hash.clone());
        let object_path = project_dir
            .join(REWIND_DIR)
            .join("objects")
            .join(&entry.hash);
        if !object_path.exists() {
            report.error(format!(
                "Missing object for hash {} referenced by snapshot {}: {}",
                entry.hash, snapshot_id, file_path
            ));
            continue;
        }

        match fs::read(&object_path) {
            Ok(bytes) => {
                let actual_hash = sha256_hex(&bytes);
                if actual_hash != entry.hash {
                    report.error(format!(
                        "Object hash mismatch for {}: expected {}, got {}",
                        file_path, entry.hash, actual_hash
                    ));
                }
                if bytes.len() as u64 != entry.size {
                    report.error(format!(
                        "Object size mismatch for {} in snapshot {}: expected {}, got {}",
                        file_path,
                        snapshot_id,
                        entry.size,
                        bytes.len()
                    ));
                }
                if first_reference {
                    report.stats.reachable_object_bytes += bytes.len() as u64;
                }
            }
            Err(error) => report.error(format!(
                "Could not read object {} referenced by snapshot {}: {}",
                entry.hash, snapshot_id, error
            )),
        }
    }
}

fn verify_snapshot_manifest(
    expected_snapshot_id: &str,
    snapshot: &SnapshotManifest,
    report: &mut IntegrityReport,
) {
    if snapshot.id != expected_snapshot_id {
        report.error(format!(
            "Snapshot file {expected_snapshot_id} contains manifest id {}",
            snapshot.id
        ));
    }

    let computed = compute_snapshot_id_for_manifest(snapshot);
    if computed != snapshot.id {
        report.error(format!(
            "Snapshot {} content id mismatch: expected {}, computed {}",
            snapshot.id, snapshot.id, computed
        ));
    }

    for directory in &snapshot.directories {
        if let Err(error) = crate::path_safety::validate_relative_path(directory) {
            report.error(format!(
                "Invalid directory path in snapshot {}: {} ({error})",
                snapshot.id, directory
            ));
        }
    }
    for file in snapshot.files.keys() {
        if let Err(error) = crate::path_safety::validate_relative_path(file) {
            report.error(format!(
                "Invalid file path in snapshot {}: {} ({error})",
                snapshot.id, file
            ));
        }
    }
    for symlink in snapshot.symlinks.keys() {
        if let Err(error) = crate::path_safety::validate_relative_path(symlink) {
            report.error(format!(
                "Invalid symlink path in snapshot {}: {} ({error})",
                snapshot.id, symlink
            ));
        }
    }
    if let Err(error) = validate_snapshot_paths(
        snapshot.directories.iter(),
        snapshot.files.keys().chain(snapshot.symlinks.keys()),
    ) {
        report.error(format!(
            "Snapshot {} has invalid paths: {error}",
            snapshot.id
        ));
    }
    if snapshot.manifest_version == 0 || snapshot.manifest_version > 2 {
        report.error(format!(
            "Snapshot {} has unsupported manifest_version {}",
            snapshot.id, snapshot.manifest_version
        ));
    }
}

fn read_checkpoints_raw(conn: &rusqlite::Connection) -> Result<Vec<history::Checkpoint>> {
    let mut stmt = conn.prepare(
        "SELECT name, snapshot_id, message, created_at
         FROM checkpoints
         ORDER BY name ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(history::Checkpoint {
            name: row.get(0)?,
            snapshot_id: row.get(1)?,
            message: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading checkpoints")
}

fn read_trace_ids(conn: &rusqlite::Connection) -> Result<BTreeSet<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM command_traces ORDER BY id ASC")?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    rows.collect::<rusqlite::Result<BTreeSet<_>>>()
        .context("reading trace ids")
}

fn list_snapshot_ids(project_dir: &Path) -> Result<BTreeSet<String>> {
    let snapshots_dir = project_dir.join(REWIND_DIR).join("snapshots");
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(&snapshots_dir)
        .with_context(|| format!("reading {}", snapshots_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            ids.insert(stem.to_owned());
        }
    }
    Ok(ids)
}

fn list_object_files(project_dir: &Path) -> Result<BTreeMap<String, ObjectFile>> {
    let objects_dir = project_dir.join(REWIND_DIR).join("objects");
    let mut objects = BTreeMap::new();
    if !objects_dir.exists() {
        return Ok(objects);
    }

    for entry in WalkDir::new(&objects_dir).min_depth(1) {
        let entry = entry.with_context(|| format!("walking {}", objects_dir.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().to_path_buf();
        let Some(hash) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let metadata =
            fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        objects.insert(
            hash.to_owned(),
            ObjectFile {
                path,
                size: metadata.len(),
            },
        );
    }
    Ok(objects)
}

fn remove_empty_object_dirs(objects_dir: &Path) -> Result<()> {
    if !objects_dir.exists() {
        return Ok(());
    }

    let mut dirs = WalkDir::new(objects_dir)
        .min_depth(1)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir())
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    for dir in dirs {
        if fs::read_dir(&dir)?.next().is_none() {
            fs::remove_dir(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
    }
    Ok(())
}

impl IntegrityReport {
    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(IntegrityIssue {
            message: message.into(),
        });
    }

    fn warning(&mut self, message: impl Into<String>) {
        self.warnings.push(IntegrityIssue {
            message: message.into(),
        });
    }
}
