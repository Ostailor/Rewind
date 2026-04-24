use crate::diff::{diff_snapshots, ChangeType, FileChange, SnapshotDiff};
use crate::forensics::{self, PathHistoryEntry};
use crate::history::{self, Event};
use crate::path_safety::validate_relative_path;
use crate::snapshot::load_snapshot;
use crate::trace::{self, TraceDetails, TraceFileEvent};
use anyhow::{Context, Result};
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ProvenanceEvent {
    pub event: Event,
    pub diff: SnapshotDiff,
    pub trace: Option<TraceDetails>,
    pub correlation: Correlation,
}

#[derive(Debug, Clone, Default)]
pub struct Correlation {
    pub changed_and_traced: Vec<String>,
    pub changed_but_not_traced: Vec<String>,
    pub traced_but_unchanged: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WhyResult {
    pub path: String,
    pub current_state: PathState,
    pub last_change: Option<PathHistoryEntry>,
    pub trace_accesses: Vec<TraceFileEvent>,
}

#[derive(Debug, Clone)]
pub enum PathState {
    File { hash: String, size: u64 },
    Directory,
    Missing,
}

#[derive(Debug, Clone)]
pub struct ImpactEntry {
    pub event_id: i64,
    pub timestamp: String,
    pub command: String,
    pub access_kind: String,
    pub operation: String,
    pub final_change: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImpactResult {
    pub path: String,
    pub entries: Vec<ImpactEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct ProvenanceStats {
    pub traced_events_with_file_access: usize,
    pub paths_with_trace_access: usize,
    pub events_with_trace_and_changes: usize,
    pub events_with_changes_but_no_trace: usize,
}

pub fn explain_event(project_dir: &Path, event_id: i64) -> Result<ProvenanceEvent> {
    let conn = history::ensure_initialized(project_dir)?;
    let event = history::get_event(&conn, event_id)?
        .with_context(|| format!("event {event_id} not found"))?;
    let before = load_snapshot(project_dir, &event.before_snapshot)?;
    let after = load_snapshot(project_dir, &event.after_snapshot)?;
    let diff = diff_snapshots(&before, &after);
    let trace = trace::trace_details(project_dir, event_id)?;
    let correlation = correlate(&diff.changes, trace.as_ref());
    Ok(ProvenanceEvent {
        event,
        diff,
        trace,
        correlation,
    })
}

pub fn why_path(project_dir: &Path, path: &str) -> Result<WhyResult> {
    let path = normalize_path(path)?;
    let conn = history::ensure_initialized(project_dir)?;
    let head = history::get_head_snapshot(&conn)?.context("workspace has no head snapshot")?;
    let snapshot = load_snapshot(project_dir, &head)?;
    let current_state = if let Some(file) = snapshot.files.get(&path) {
        PathState::File {
            hash: file.hash.clone(),
            size: file.size,
        }
    } else if snapshot.directories.iter().any(|dir| dir == &path) {
        PathState::Directory
    } else {
        PathState::Missing
    };

    let last_change = forensics::path_history(project_dir, &path, None)?
        .into_iter()
        .last();
    let trace_accesses = if let Some(change) = &last_change {
        trace_accesses_for_event_path(project_dir, change.event_id, &path)?
    } else {
        Vec::new()
    };
    Ok(WhyResult {
        path,
        current_state,
        last_change,
        trace_accesses,
    })
}

pub fn impact_path(
    project_dir: &Path,
    path: &str,
    since: Option<i64>,
    until: Option<i64>,
) -> Result<ImpactResult> {
    let path = normalize_path(path)?;
    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?
        .into_iter()
        .map(|event| (event.id, event))
        .collect::<BTreeMap<_, _>>();
    let mut stmt = conn.prepare(
        "SELECT command_traces.event_id, trace_file_events.operation, trace_file_events.access_kind
         FROM trace_file_events
         JOIN command_traces ON command_traces.id = trace_file_events.trace_id
         WHERE trace_file_events.within_workspace = 1
           AND (trace_file_events.path = ?1 OR trace_file_events.path LIKE ?2)
         ORDER BY command_traces.event_id ASC, trace_file_events.seq ASC",
    )?;
    let prefix = format!("{path}/%");
    let rows = stmt.query_map(params![path, prefix], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (event_id, operation, access_kind) = row?;
        if since.is_some_and(|since| event_id < since)
            || until.is_some_and(|until| event_id > until)
        {
            continue;
        }
        let Some(event) = events.get(&event_id) else {
            continue;
        };
        let final_change = final_change_for_path(&conn, event_id, &path)?;
        entries.push(ImpactEntry {
            event_id,
            timestamp: event.timestamp.clone(),
            command: event.command.clone(),
            access_kind,
            operation,
            final_change,
        });
    }
    Ok(ImpactResult { path, entries })
}

pub fn provenance_stats(project_dir: &Path) -> Result<ProvenanceStats> {
    let conn = history::ensure_initialized(project_dir)?;
    let traced_events_with_file_access: usize = conn.query_row(
        "SELECT COUNT(DISTINCT command_traces.event_id)
         FROM command_traces
         JOIN trace_file_events ON trace_file_events.trace_id = command_traces.id",
        [],
        |row| row.get(0),
    )?;
    let paths_with_trace_access: usize = conn.query_row(
        "SELECT COUNT(DISTINCT path)
         FROM trace_file_events
         WHERE within_workspace = 1 AND path IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    let events_with_trace_and_changes: usize = conn.query_row(
        "SELECT COUNT(DISTINCT events.id)
         FROM events
         JOIN file_changes ON file_changes.event_id = events.id
         JOIN command_traces ON command_traces.event_id = events.id",
        [],
        |row| row.get(0),
    )?;
    let events_with_changes_but_no_trace: usize = conn.query_row(
        "SELECT COUNT(DISTINCT events.id)
         FROM events
         JOIN file_changes ON file_changes.event_id = events.id
         LEFT JOIN command_traces ON command_traces.event_id = events.id
         WHERE command_traces.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    Ok(ProvenanceStats {
        traced_events_with_file_access,
        paths_with_trace_access,
        events_with_trace_and_changes,
        events_with_changes_but_no_trace,
    })
}

pub fn trace_accesses_for_event_path(
    project_dir: &Path,
    event_id: i64,
    path: &str,
) -> Result<Vec<TraceFileEvent>> {
    let path = normalize_path(path)?;
    let Some(details) = trace::trace_details(project_dir, event_id)? else {
        return Ok(Vec::new());
    };
    Ok(details
        .files
        .into_iter()
        .filter(|event| event.within_workspace)
        .filter(|event| {
            event.path.as_deref() == Some(path.as_str())
                || event
                    .path
                    .as_deref()
                    .and_then(|candidate| candidate.strip_prefix(&path))
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .collect())
}

fn correlate(changes: &[FileChange], trace: Option<&TraceDetails>) -> Correlation {
    let changed = changes
        .iter()
        .map(|change| change.path.clone())
        .collect::<BTreeSet<_>>();
    let traced = trace
        .map(|trace| {
            trace
                .files
                .iter()
                .filter(|event| event.within_workspace)
                .filter_map(|event| event.path.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();

    Correlation {
        changed_and_traced: changed.intersection(&traced).cloned().collect(),
        changed_but_not_traced: changed.difference(&traced).cloned().collect(),
        traced_but_unchanged: traced.difference(&changed).cloned().collect(),
    }
}

fn final_change_for_path(
    conn: &rusqlite::Connection,
    event_id: i64,
    path: &str,
) -> Result<Option<String>> {
    history::list_changes(conn, event_id).map(|changes| {
        changes
            .into_iter()
            .find(|change| change.path == path)
            .map(|change| match change.change_type {
                ChangeType::Created => "created".to_owned(),
                ChangeType::Modified => "modified".to_owned(),
                ChangeType::Deleted => "deleted".to_owned(),
            })
    })
}

fn normalize_path(path: &str) -> Result<String> {
    Ok(validate_relative_path(path)?
        .to_string_lossy()
        .replace('\\', "/"))
}
