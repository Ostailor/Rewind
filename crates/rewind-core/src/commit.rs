use crate::diff::SnapshotDiff;
use crate::history;
use crate::snapshot::{create_snapshot, load_snapshot, write_snapshot};
use crate::status::compare_current_to_head;
use crate::transaction;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum CommitOutcome {
    Committed { event_id: i64, diff: SnapshotDiff },
    DryRun { diff: SnapshotDiff },
    Clean,
}

pub fn commit_worktree(project_dir: &Path, message: &str, dry_run: bool) -> Result<CommitOutcome> {
    let conn = history::ensure_initialized(project_dir)?;
    transaction::ensure_no_active(project_dir)?;
    let head_snapshot = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let head = load_snapshot(project_dir, &head_snapshot)?;
    let status = compare_current_to_head(project_dir, &head_snapshot, &head)?;
    let diff = status.diff;

    if diff.changes.is_empty() && diff.added_dirs.is_empty() && diff.deleted_dirs.is_empty() {
        return Ok(CommitOutcome::Clean);
    }

    if dry_run {
        return Ok(CommitOutcome::DryRun { diff });
    }

    let after = create_snapshot(project_dir)?;
    write_snapshot(project_dir, &after)?;

    let timestamp = Utc::now().to_rfc3339();
    let command = format!("commit: {message}");
    let mut conn = conn;
    let event_id = history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "commit",
            started_dirty: false,
            timestamp: &timestamp,
            command: &command,
            command_argv_json: None,
            command_cwd_relative: ".",
            exit_code: 0,
            before_snapshot: &head_snapshot,
            after_snapshot: &after.id,
            diff: &diff,
            transaction_id: None,
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;

    Ok(CommitOutcome::Committed { event_id, diff })
}
