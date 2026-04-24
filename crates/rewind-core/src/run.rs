use crate::diff::diff_snapshots;
use crate::history;
use crate::snapshot::{create_snapshot, write_snapshot};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::Path;
use std::process::Command;

pub fn run_command(project_dir: &Path, command: &[String]) -> Result<i32> {
    if command.is_empty() {
        bail!("missing command after `rewind run --`");
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
    let mut conn = history::ensure_initialized(project_dir)?;
    let timestamp = Utc::now().to_rfc3339();
    let command = command_string(command);
    history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "run",
            timestamp: &timestamp,
            command: &command,
            exit_code,
            before_snapshot: &before.id,
            after_snapshot: &after.id,
            diff: &diff,
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;

    Ok(exit_code)
}

pub fn command_string(command: &[String]) -> String {
    command.join(" ")
}
