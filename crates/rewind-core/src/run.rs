use crate::diff::diff_snapshots;
use crate::history;
use crate::snapshot::{create_snapshot, load_snapshot, write_snapshot};
use crate::status::{compare_current_to_head, WorktreeStatus};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Ran { exit_code: i32 },
    Dirty { status: WorktreeStatus },
}

pub fn run_command(
    project_dir: &Path,
    command: &[String],
    allow_dirty: bool,
) -> Result<RunOutcome> {
    if command.is_empty() {
        bail!("missing command after `rewind run --`");
    }

    let conn = history::ensure_initialized(project_dir)?;
    let head_snapshot = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let head = load_snapshot(project_dir, &head_snapshot)?;
    let worktree_status = compare_current_to_head(project_dir, &head_snapshot, &head)?;
    let started_dirty = !worktree_status.is_clean();
    if started_dirty && !allow_dirty {
        return Ok(RunOutcome::Dirty {
            status: worktree_status,
        });
    }

    let before = create_snapshot(project_dir).context("creating before snapshot")?;
    write_snapshot(project_dir, &before).context("writing before snapshot")?;

    let status = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(project_dir)
        .status()
        .with_context(|| format!("running command `{}`", command_string(command)))?;
    let exit_code = status.code().unwrap_or(1);

    let after = create_snapshot(project_dir).context("creating after snapshot")?;
    write_snapshot(project_dir, &after).context("writing after snapshot")?;

    let diff = diff_snapshots(&before, &after);
    let mut conn = conn;
    let timestamp = Utc::now().to_rfc3339();
    let command = command_string(command);
    history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "run",
            started_dirty,
            timestamp: &timestamp,
            command: &command,
            exit_code,
            before_snapshot: &before.id,
            after_snapshot: &after.id,
            diff: &diff,
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;

    Ok(RunOutcome::Ran { exit_code })
}

pub fn command_string(command: &[String]) -> String {
    command.join(" ")
}
