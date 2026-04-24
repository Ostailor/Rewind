use crate::history::{self, Checkpoint};
use crate::status;
use crate::transaction;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone)]
pub enum CheckpointCreateOutcome {
    Created,
    Dirty { status: status::WorktreeStatus },
}

pub fn create_checkpoint(
    project_dir: &Path,
    name: &str,
    message: &str,
    force: bool,
) -> Result<CheckpointCreateOutcome> {
    validate_checkpoint_name(name)?;
    transaction::ensure_no_active(project_dir)?;
    let status = status::worktree_status(project_dir)?;
    if !status.is_clean() {
        return Ok(CheckpointCreateOutcome::Dirty { status });
    }

    let conn = history::ensure_initialized(project_dir)?;
    let head = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let created_at = Utc::now().to_rfc3339();

    if force {
        conn.execute(
            "INSERT INTO checkpoints (name, snapshot_id, message, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
                snapshot_id = excluded.snapshot_id,
                message = excluded.message,
                created_at = excluded.created_at",
            params![name, head, message, created_at],
        )
        .with_context(|| format!("creating checkpoint {name}"))?;
    } else {
        conn.execute(
            "INSERT INTO checkpoints (name, snapshot_id, message, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![name, head, message, created_at],
        )
        .with_context(|| format!("creating checkpoint {name}"))?;
    }

    Ok(CheckpointCreateOutcome::Created)
}

pub fn list_checkpoints(project_dir: &Path) -> Result<Vec<Checkpoint>> {
    let conn = history::ensure_initialized(project_dir)?;
    let mut stmt = conn
        .prepare(
            "SELECT name, snapshot_id, message, created_at
             FROM checkpoints
             ORDER BY name ASC",
        )
        .context("preparing checkpoint list query")?;
    let rows = stmt.query_map([], checkpoint_from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading checkpoints")
}

pub fn get_checkpoint(project_dir: &Path, name: &str) -> Result<Option<Checkpoint>> {
    validate_checkpoint_name(name)?;
    let conn = history::ensure_initialized(project_dir)?;
    conn.query_row(
        "SELECT name, snapshot_id, message, created_at
         FROM checkpoints
         WHERE name = ?1",
        [name],
        checkpoint_from_row,
    )
    .optional()
    .with_context(|| format!("reading checkpoint {name}"))
}

pub fn delete_checkpoint(project_dir: &Path, name: &str) -> Result<bool> {
    validate_checkpoint_name(name)?;
    transaction::ensure_no_active(project_dir)?;
    let conn = history::ensure_initialized(project_dir)?;
    let changed = conn
        .execute("DELETE FROM checkpoints WHERE name = ?1", [name])
        .with_context(|| format!("deleting checkpoint {name}"))?;
    Ok(changed > 0)
}

pub fn validate_checkpoint_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("checkpoint name must not be empty");
    }
    if name.len() > 80 {
        bail!("checkpoint name must be 80 characters or fewer");
    }
    if name.contains("..") {
        bail!("checkpoint name must not contain `..`");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("checkpoint name may contain only ASCII letters, numbers, `_`, `-`, and `.`");
    }
    Ok(())
}

fn checkpoint_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    Ok(Checkpoint {
        name: row.get(0)?,
        snapshot_id: row.get(1)?,
        message: row.get(2)?,
        created_at: row.get(3)?,
    })
}
