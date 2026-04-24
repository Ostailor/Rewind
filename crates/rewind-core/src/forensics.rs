use crate::checkpoint;
use crate::history::{self, Event};
use crate::integrity;
use crate::object_store::ObjectStore;
use crate::path_safety::validate_relative_path;
use crate::snapshot::load_snapshot;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

const MAX_TEXT_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PathHistoryEntry {
    pub event_id: i64,
    pub timestamp: String,
    pub kind: String,
    pub command: String,
    pub change_type: String,
    pub path: String,
    pub undone: bool,
    pub started_dirty: bool,
}

#[derive(Debug, Clone)]
pub enum CatTarget {
    BeforeEvent(i64),
    AfterEvent(i64),
    Snapshot(String),
    Checkpoint(String),
}

#[derive(Debug, Clone)]
pub struct CatFile {
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct DeletedFileEntry {
    pub path: String,
    pub deleted_by_event_id: Option<i64>,
    pub suggested_restore: Option<String>,
}

#[derive(Debug, Clone)]
pub enum GrepTarget {
    Snapshot(String),
    Checkpoint(String),
    History,
}

#[derive(Debug, Clone)]
pub struct GrepOptions {
    pub ignore_case: bool,
    pub max_results: usize,
    pub max_file_size: u64,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            ignore_case: false,
            max_results: 200,
            max_file_size: MAX_TEXT_FILE_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GrepResult {
    pub matches: Vec<GrepMatch>,
    pub limit_reached: bool,
}

#[derive(Debug, Clone)]
pub struct GrepMatch {
    pub snapshot_id: String,
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

pub fn path_history(
    project_dir: &Path,
    path: &str,
    limit: Option<usize>,
) -> Result<Vec<PathHistoryEntry>> {
    let path = normalized_path(path)?;
    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?
        .into_iter()
        .map(|event| (event.id, event))
        .collect::<BTreeMap<_, _>>();
    let mut stmt = conn.prepare(
        "SELECT event_id, path, change_type
         FROM file_changes
         ORDER BY event_id ASC, path ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        let (event_id, changed_path, change_type) = row?;
        if !matches_path(&path, &changed_path) {
            continue;
        }
        let Some(event) = events.get(&event_id) else {
            continue;
        };
        entries.push(PathHistoryEntry {
            event_id,
            timestamp: event.timestamp.clone(),
            kind: event.kind.clone(),
            command: event.command.clone(),
            change_type,
            path: changed_path,
            undone: event.undone,
            started_dirty: event.started_dirty,
        });
        if limit.is_some_and(|limit| entries.len() >= limit) {
            break;
        }
    }

    Ok(entries)
}

pub fn cat_file(project_dir: &Path, path: &str, target: CatTarget) -> Result<CatFile> {
    let path = normalized_path(path)?;
    let snapshot_id = resolve_cat_snapshot(project_dir, target)?;
    let snapshot = load_snapshot(project_dir, &snapshot_id)?;

    if snapshot.directories.contains(&path) {
        bail!("Path is a directory in that snapshot.");
    }
    if let Some(symlink) = snapshot.symlinks.get(&path) {
        bail!("Path is a symlink in that snapshot: {}", symlink.target);
    }
    let entry = snapshot
        .files
        .get(&path)
        .with_context(|| format!("Path did not exist in snapshot {snapshot_id}.",))?;
    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    let bytes = std::fs::read(object_store.object_path(&entry.hash))
        .with_context(|| format!("reading object {}", entry.hash))?;
    Ok(CatFile { bytes })
}

pub fn deleted_files(
    project_dir: &Path,
    path_filter: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<DeletedFileEntry>> {
    let filter = path_filter.map(normalized_path).transpose()?;
    let stats = integrity::stats(project_dir)?;
    let head = load_snapshot(project_dir, &stats.head_snapshot)?;
    let mut historical_paths = BTreeSet::new();
    for snapshot_id in &stats.reachable_snapshots {
        let snapshot = load_snapshot(project_dir, snapshot_id)?;
        for path in snapshot.files.keys() {
            if filter
                .as_ref()
                .is_none_or(|filter| matches_path(filter, path))
            {
                historical_paths.insert(path.clone());
            }
        }
        for path in snapshot.symlinks.keys() {
            if filter
                .as_ref()
                .is_none_or(|filter| matches_path(filter, path))
            {
                historical_paths.insert(path.clone());
            }
        }
    }

    let deletion_events = deletion_events(project_dir)?;
    let mut entries = Vec::new();
    for path in historical_paths {
        if head.files.contains_key(&path) || head.symlinks.contains_key(&path) {
            continue;
        }
        let deleted_by_event_id = deletion_events.get(&path).copied();
        let suggested_restore = deleted_by_event_id
            .map(|event_id| format!("rewind restore {path} --before {event_id}"));
        entries.push(DeletedFileEntry {
            path,
            deleted_by_event_id,
            suggested_restore,
        });
        if limit.is_some_and(|limit| entries.len() >= limit) {
            break;
        }
    }
    Ok(entries)
}

pub fn grep(
    project_dir: &Path,
    pattern: &str,
    target: GrepTarget,
    options: GrepOptions,
) -> Result<GrepResult> {
    let snapshot_ids = match target {
        GrepTarget::Snapshot(prefix) => {
            vec![integrity::resolve_snapshot_prefix(project_dir, &prefix)?]
        }
        GrepTarget::Checkpoint(name) => {
            let checkpoint = checkpoint::get_checkpoint(project_dir, &name)?
                .with_context(|| format!("checkpoint {name} not found"))?;
            vec![checkpoint.snapshot_id]
        }
        GrepTarget::History => integrity::stats(project_dir)?
            .reachable_snapshots
            .into_iter()
            .collect(),
    };

    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    let mut object_cache = HashMap::<String, Option<Vec<(usize, String)>>>::new();
    let mut matches = Vec::new();
    let needle = if options.ignore_case {
        pattern.to_lowercase()
    } else {
        pattern.to_owned()
    };

    for snapshot_id in snapshot_ids {
        let snapshot = load_snapshot(project_dir, &snapshot_id)?;
        for (path, entry) in snapshot.files {
            let object_matches = if let Some(cached) = object_cache.get(&entry.hash) {
                cached.clone()
            } else {
                let value = search_object(
                    &object_store.object_path(&entry.hash),
                    &needle,
                    options.ignore_case,
                    options.max_file_size,
                )?;
                object_cache.insert(entry.hash.clone(), value.clone());
                value
            };
            let Some(object_matches) = object_matches else {
                continue;
            };
            for (line_number, line) in object_matches {
                matches.push(GrepMatch {
                    snapshot_id: snapshot_id.clone(),
                    path: path.clone(),
                    line_number,
                    line,
                });
                if matches.len() >= options.max_results {
                    return Ok(GrepResult {
                        matches,
                        limit_reached: true,
                    });
                }
            }
        }
    }

    Ok(GrepResult {
        matches,
        limit_reached: false,
    })
}

pub fn changed_paths_for_event(project_dir: &Path, event_id: i64) -> Result<Vec<String>> {
    let conn = history::ensure_initialized(project_dir)?;
    let changes = history::list_changes(&conn, event_id)?;
    Ok(changes.into_iter().map(|change| change.path).collect())
}

fn resolve_cat_snapshot(project_dir: &Path, target: CatTarget) -> Result<String> {
    let conn = history::ensure_initialized(project_dir)?;
    match target {
        CatTarget::BeforeEvent(event_id) => {
            Ok(event(project_dir, &conn, event_id)?.before_snapshot)
        }
        CatTarget::AfterEvent(event_id) => Ok(event(project_dir, &conn, event_id)?.after_snapshot),
        CatTarget::Snapshot(prefix) => integrity::resolve_snapshot_prefix(project_dir, &prefix),
        CatTarget::Checkpoint(name) => {
            let checkpoint = checkpoint::get_checkpoint(project_dir, &name)?
                .with_context(|| format!("checkpoint {name} not found"))?;
            Ok(checkpoint.snapshot_id)
        }
    }
}

fn event(project_dir: &Path, conn: &rusqlite::Connection, event_id: i64) -> Result<Event> {
    history::get_event(conn, event_id)?
        .with_context(|| format!("event {event_id} not found in {}", project_dir.display()))
}

fn normalized_path(path: &str) -> Result<String> {
    let path = validate_relative_path(path)?;
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn matches_path(requested: &str, changed: &str) -> bool {
    changed == requested
        || changed
            .strip_prefix(requested)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn deletion_events(project_dir: &Path) -> Result<BTreeMap<String, i64>> {
    let conn = history::ensure_initialized(project_dir)?;
    let mut stmt = conn.prepare(
        "SELECT event_id, path
         FROM file_changes
         WHERE change_type = 'deleted'
         ORDER BY event_id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut deleted = BTreeMap::new();
    for row in rows {
        let (event_id, path) = row?;
        deleted.insert(path, event_id);
    }
    Ok(deleted)
}

fn search_object(
    object_path: &PathBuf,
    needle: &str,
    ignore_case: bool,
    max_file_size: u64,
) -> Result<Option<Vec<(usize, String)>>> {
    let metadata = std::fs::metadata(object_path)?;
    if metadata.len() > max_file_size {
        return Ok(None);
    }
    let bytes = std::fs::read(object_path)?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    let mut matches = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let haystack = if ignore_case {
            line.to_lowercase()
        } else {
            line.to_owned()
        };
        if haystack.contains(needle) {
            matches.push((index + 1, line.to_owned()));
        }
    }
    Ok(Some(matches))
}
