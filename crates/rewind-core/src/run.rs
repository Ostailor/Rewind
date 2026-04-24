use crate::diff::diff_snapshots;
use crate::history;
use crate::snapshot::{create_snapshot, load_snapshot, write_snapshot};
use crate::status::{compare_current_to_head, WorktreeStatus};
use crate::trace::{self, TraceMode, TracePlan};
use crate::transaction;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone)]
pub enum RunOutcome {
    Ran {
        exit_code: i32,
        trace_unavailable: Option<String>,
    },
    Dirty {
        status: WorktreeStatus,
    },
}

pub fn run_command(
    project_dir: &Path,
    command: &[String],
    allow_dirty: bool,
) -> Result<RunOutcome> {
    run_command_with_trace(project_dir, command, allow_dirty, TraceMode::Off, false)
}

pub fn run_command_with_trace(
    project_dir: &Path,
    command: &[String],
    allow_dirty: bool,
    trace_mode: TraceMode,
    trace_keep_raw: bool,
) -> Result<RunOutcome> {
    if command.is_empty() {
        bail!("missing command after `rewind run --`");
    }

    let conn = history::ensure_initialized(project_dir)?;
    transaction::ensure_no_active(project_dir)?;
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

    let trace_plan = trace::prepare(project_dir, trace_mode)?;
    let status = match &trace_plan {
        TracePlan::Off | TracePlan::Unavailable { .. } => Command::new(&command[0])
            .args(&command[1..])
            .current_dir(project_dir)
            .status()
            .with_context(|| format!("running command `{}`", command_string(command)))?,
        TracePlan::Strace { output_path, .. } => {
            let mut traced = trace::strace_command(output_path, command);
            traced
                .current_dir(project_dir)
                .status()
                .with_context(|| format!("running traced command `{}`", command_string(command)))?
        }
    };
    let exit_code = status.code().unwrap_or(1);

    let after = create_snapshot(project_dir).context("creating after snapshot")?;
    write_snapshot(project_dir, &after).context("writing after snapshot")?;

    let diff = diff_snapshots(&before, &after);
    let mut conn = conn;
    let timestamp = Utc::now().to_rfc3339();
    let command_argv_json = serde_json::to_string(command).context("serializing command argv")?;
    let command = command_string(command);
    let event_id = history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "run",
            started_dirty,
            timestamp: &timestamp,
            command: &command,
            command_argv_json: Some(&command_argv_json),
            command_cwd_relative: ".",
            exit_code,
            before_snapshot: &before.id,
            after_snapshot: &after.id,
            diff: &diff,
            transaction_id: None,
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;

    let trace_unavailable = match trace_plan {
        TracePlan::Off => None,
        TracePlan::Unavailable {
            tracer,
            reason,
            started_at,
        } => {
            trace::record_unavailable(&conn, event_id, &tracer, &reason, &started_at)?;
            Some(reason)
        }
        TracePlan::Strace {
            output_path,
            started_at,
        } => match trace::record_captured(
            &conn,
            project_dir,
            event_id,
            &output_path,
            &started_at,
            trace_keep_raw,
        ) {
            Ok(_) => None,
            Err(error) => {
                trace::record_parse_error(&conn, event_id, &output_path, &started_at, &error)?;
                None
            }
        },
    };

    Ok(RunOutcome::Ran {
        exit_code,
        trace_unavailable,
    })
}

pub fn command_string(command: &[String]) -> String {
    command.join(" ")
}
