use crate::checkpoint;
use crate::diff::{diff_snapshots, ChangeType, FileChange, SnapshotDiff};
use crate::history::{self, Event};
use crate::integrity::StorageStats;
use crate::object_store::ObjectStore;
use crate::snapshot::load_snapshot;
use crate::status;
use crate::REWIND_DIR;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

const MAX_TEXT_DIFF_BYTES: u64 = 1024 * 1024;
const MAX_PREVIEW_LINES: usize = 80;

#[derive(Debug, Clone)]
pub struct TuiModel {
    pub head_snapshot: String,
    pub worktree: TuiWorktreeStatus,
    pub events: Vec<TuiEvent>,
    pub selected_event_id: Option<i64>,
    pub selected_event: Option<TuiSelectedEvent>,
    pub checkpoints: Vec<TuiCheckpoint>,
    pub stats: StorageStats,
    pub recovery_needed: bool,
}

#[derive(Debug, Clone)]
pub struct TuiWorktreeStatus {
    pub clean: bool,
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub added_dirs: usize,
    pub deleted_dirs: usize,
}

#[derive(Debug, Clone)]
pub struct TuiEvent {
    pub id: i64,
    pub kind: String,
    pub command: String,
    pub before_snapshot: String,
    pub after_snapshot: String,
    pub undone: bool,
    pub started_dirty: bool,
}

#[derive(Debug, Clone)]
pub struct TuiSelectedEvent {
    pub event: TuiEvent,
    pub diff: Option<SnapshotDiff>,
    pub preview_lines: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TuiCheckpoint {
    pub name: String,
    pub snapshot_id: String,
    pub message: String,
    pub points_to_head: bool,
}

pub fn build_model(project_dir: &Path, selected_event_id: Option<i64>) -> Result<TuiModel> {
    let conn = history::ensure_initialized(project_dir)?;
    let head_snapshot = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let events = history::list_events(&conn)?
        .into_iter()
        .map(tui_event)
        .collect::<Vec<_>>();
    let selected_id = selected_event_id.or_else(|| events.last().map(|event| event.id));
    let selected_event = if let Some(event_id) = selected_id {
        let event = events
            .iter()
            .find(|event| event.id == event_id)
            .cloned()
            .with_context(|| format!("event {event_id} not found"))?;
        Some(selected_event(project_dir, event))
    } else {
        None
    };
    let worktree = worktree_status(project_dir)?;
    let checkpoints = checkpoint::list_checkpoints(project_dir)?
        .into_iter()
        .map(|checkpoint| TuiCheckpoint {
            points_to_head: checkpoint.snapshot_id == head_snapshot,
            name: checkpoint.name,
            snapshot_id: checkpoint.snapshot_id,
            message: checkpoint.message,
        })
        .collect::<Vec<_>>();
    let stats = crate::integrity::stats(project_dir)?;
    let recovery_needed = stats.active_journal;

    Ok(TuiModel {
        head_snapshot,
        worktree,
        events,
        selected_event_id: selected_id,
        selected_event,
        checkpoints,
        stats,
        recovery_needed,
    })
}

fn tui_event(event: Event) -> TuiEvent {
    TuiEvent {
        id: event.id,
        kind: event.kind,
        command: event.command,
        before_snapshot: event.before_snapshot,
        after_snapshot: event.after_snapshot,
        undone: event.undone,
        started_dirty: event.started_dirty,
    }
}

fn worktree_status(project_dir: &Path) -> Result<TuiWorktreeStatus> {
    let status = status::worktree_status(project_dir)?;
    Ok(TuiWorktreeStatus {
        clean: status.is_clean(),
        added: status.added_files().len(),
        modified: status.modified_files().len(),
        deleted: status.deleted_files().len(),
        added_dirs: status.diff.added_dirs.len(),
        deleted_dirs: status.diff.deleted_dirs.len(),
    })
}

fn selected_event(project_dir: &Path, event: TuiEvent) -> TuiSelectedEvent {
    match load_selected_diff(project_dir, &event) {
        Ok((diff, preview_lines)) => TuiSelectedEvent {
            event,
            diff: Some(diff),
            preview_lines,
            error: None,
        },
        Err(error) => TuiSelectedEvent {
            event,
            diff: None,
            preview_lines: Vec::new(),
            error: Some(format!("Unable to load diff: {error:#}")),
        },
    }
}

fn load_selected_diff(project_dir: &Path, event: &TuiEvent) -> Result<(SnapshotDiff, Vec<String>)> {
    let before = load_snapshot(project_dir, &event.before_snapshot)?;
    let after = load_snapshot(project_dir, &event.after_snapshot)?;
    let diff = diff_snapshots(&before, &after);
    let mut lines = change_summary_lines(&diff);

    if diff
        .changes
        .iter()
        .any(|change| change.change_type == ChangeType::Modified)
    {
        lines.push("Diff:".to_owned());
    }
    for change in diff
        .changes
        .iter()
        .filter(|change| change.change_type == ChangeType::Modified)
    {
        append_text_diff(project_dir, event.id, change, &mut lines)?;
        if lines.len() > MAX_PREVIEW_LINES {
            lines.truncate(MAX_PREVIEW_LINES);
            lines.push(format!(
                "Diff truncated. Run rewind diff {} for full output.",
                event.id
            ));
            break;
        }
    }

    Ok((diff, lines))
}

fn change_summary_lines(diff: &SnapshotDiff) -> Vec<String> {
    let mut lines = Vec::new();
    append_change_group(&mut lines, "Created", &diff.changes, ChangeType::Created);
    append_change_group(&mut lines, "Modified", &diff.changes, ChangeType::Modified);
    append_change_group(&mut lines, "Deleted", &diff.changes, ChangeType::Deleted);
    append_string_group(&mut lines, "Created directories", &diff.added_dirs);
    append_string_group(&mut lines, "Deleted directories", &diff.deleted_dirs);
    lines
}

fn append_change_group(
    lines: &mut Vec<String>,
    title: &str,
    changes: &[FileChange],
    change_type: ChangeType,
) {
    let paths = changes
        .iter()
        .filter(|change| change.change_type == change_type)
        .map(|change| change.path.as_str())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return;
    }

    lines.push(format!("{title}:"));
    for path in paths {
        lines.push(format!("  {path}"));
    }
}

fn append_string_group(lines: &mut Vec<String>, title: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }

    lines.push(format!("{title}:"));
    for path in paths {
        lines.push(format!("  {path}"));
    }
}

fn append_text_diff(
    project_dir: &Path,
    event_id: i64,
    change: &FileChange,
    lines: &mut Vec<String>,
) -> Result<()> {
    let Some(before_hash) = &change.before_hash else {
        return Ok(());
    };
    let Some(after_hash) = &change.after_hash else {
        return Ok(());
    };

    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    let before_path = object_store.object_path(before_hash);
    let after_path = object_store.object_path(after_hash);
    let before_meta = fs::metadata(&before_path)?;
    let after_meta = fs::metadata(&after_path)?;
    if before_meta.len() > MAX_TEXT_DIFF_BYTES || after_meta.len() > MAX_TEXT_DIFF_BYTES {
        lines.push(format!(
            "{}: content too large; textual diff skipped.",
            change.path
        ));
        return Ok(());
    }

    let before_bytes = fs::read(&before_path)?;
    let after_bytes = fs::read(&after_path)?;
    let (Ok(before_text), Ok(after_text)) = (
        String::from_utf8(before_bytes),
        String::from_utf8(after_bytes),
    ) else {
        lines.push("Binary or non-UTF8 content changed; textual diff skipped.".to_owned());
        return Ok(());
    };

    lines.push(format!("--- {} before event {}", change.path, event_id));
    lines.push(format!("+++ {} after event {}", change.path, event_id));
    for line in before_text.lines() {
        if !after_text.lines().any(|after_line| after_line == line) {
            lines.push(format!("-{line}"));
        }
    }
    for line in after_text.lines() {
        if !before_text.lines().any(|before_line| before_line == line) {
            lines.push(format!("+{line}"));
        }
    }
    Ok(())
}
