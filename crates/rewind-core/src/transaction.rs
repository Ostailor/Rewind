use crate::diff::diff_snapshots;
use crate::history;
use crate::restore::{apply_restore_plan, build_restore_plan, validate_restore_plan, RestorePlan};
use crate::snapshot::{load_snapshot, scan_worktree};
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugStop {
    None,
    AfterJournal,
    AfterApply,
    AfterCommit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreTransaction {
    pub id: String,
    pub created_at: String,
    pub operation: String,
    pub command: String,
    pub old_head_snapshot: String,
    pub target_snapshot: String,
    pub planned_event_kind: String,
    pub planned_event_command: String,
    pub transaction_id: String,
    pub phase: TransactionPhase,
    pub restore_plan: RestorePlan,
    #[serde(default)]
    pub undo_event_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionPhase {
    Prepared,
    Applying,
    Committing,
    Committed,
}

#[derive(Debug, Clone)]
pub enum RecoveryStatus {
    NoActiveTransaction,
    Active(Box<RestoreTransaction>),
}

#[derive(Debug, Clone)]
pub enum RecoverOutcome {
    NoActiveTransaction,
    Completed { id: String },
    Aborted { id: String },
}

impl RestoreTransaction {
    pub fn new(
        operation: &str,
        command: &str,
        old_head_snapshot: &str,
        target_snapshot: &str,
        planned_event_kind: &str,
        planned_event_command: &str,
        restore_plan: RestorePlan,
    ) -> Self {
        let created_at = Utc::now().to_rfc3339();
        let id = format!(
            "{}-{}",
            Utc::now().format("%Y%m%d%H%M%S%3f"),
            std::process::id()
        );
        Self {
            id: id.clone(),
            created_at,
            operation: operation.to_owned(),
            command: command.to_owned(),
            old_head_snapshot: old_head_snapshot.to_owned(),
            target_snapshot: target_snapshot.to_owned(),
            planned_event_kind: planned_event_kind.to_owned(),
            planned_event_command: planned_event_command.to_owned(),
            transaction_id: id,
            phase: TransactionPhase::Prepared,
            restore_plan,
            undo_event_id: None,
        }
    }
}

pub fn active_path(project_dir: &Path) -> PathBuf {
    project_dir
        .join(REWIND_DIR)
        .join("journal")
        .join("active.json")
}

pub fn has_active(project_dir: &Path) -> bool {
    active_path(project_dir).exists()
}

pub fn ensure_no_active(project_dir: &Path) -> Result<()> {
    if has_active(project_dir) {
        bail!("active Rewind transaction found; run `rewind recover --status` before continuing");
    }
    Ok(())
}

pub fn recovery_status(project_dir: &Path) -> Result<RecoveryStatus> {
    if !has_active(project_dir) {
        return Ok(RecoveryStatus::NoActiveTransaction);
    }
    let journal = load_active(project_dir)?;
    validate_journal_snapshots(project_dir, &journal)?;
    validate_restore_plan(project_dir, &journal.restore_plan)?;
    Ok(RecoveryStatus::Active(Box::new(journal)))
}

pub fn load_active(project_dir: &Path) -> Result<RestoreTransaction> {
    let path = active_path(project_dir);
    let bytes =
        fs::read(&path).with_context(|| format!("reading active journal {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing active journal {}", path.display()))
}

pub fn write_active(project_dir: &Path, journal: &RestoreTransaction) -> Result<()> {
    let journal_dir = project_dir.join(REWIND_DIR).join("journal");
    fs::create_dir_all(&journal_dir)
        .with_context(|| format!("creating {}", journal_dir.display()))?;
    let active = active_path(project_dir);
    let tmp = journal_dir.join("active.json.tmp");
    let bytes = serde_json::to_vec_pretty(journal).context("serializing active journal")?;
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &active)
        .with_context(|| format!("renaming {} to {}", tmp.display(), active.display()))?;
    Ok(())
}

pub fn archive_completed(project_dir: &Path) -> Result<()> {
    archive_active(project_dir, "completed")
}

pub fn archive_aborted(project_dir: &Path) -> Result<()> {
    archive_active(project_dir, "aborted")
}

pub fn complete(project_dir: &Path) -> Result<RecoverOutcome> {
    let mut journal = match recovery_status(project_dir)? {
        RecoveryStatus::NoActiveTransaction => return Ok(RecoverOutcome::NoActiveTransaction),
        RecoveryStatus::Active(journal) => journal,
    };

    restore_whole_worktree(project_dir, &journal.target_snapshot)?;
    journal.phase = TransactionPhase::Committing;
    write_active(project_dir, &journal)?;
    commit_metadata(project_dir, &journal)?;
    journal.phase = TransactionPhase::Committed;
    write_active(project_dir, &journal)?;
    let id = journal.id.clone();
    archive_completed(project_dir)?;
    Ok(RecoverOutcome::Completed { id })
}

pub fn abort(project_dir: &Path) -> Result<RecoverOutcome> {
    let journal = match recovery_status(project_dir)? {
        RecoveryStatus::NoActiveTransaction => return Ok(RecoverOutcome::NoActiveTransaction),
        RecoveryStatus::Active(journal) => journal,
    };

    let conn = history::ensure_initialized(project_dir)?;
    let head = history::get_head_snapshot(&conn)?.unwrap_or_default();
    if journal.phase == TransactionPhase::Committed || head == journal.target_snapshot {
        bail!(
            "transaction metadata is already committed; use `rewind recover --complete`, then `rewind undo` if you want to go back"
        );
    }

    restore_whole_worktree(project_dir, &journal.old_head_snapshot)?;
    history::set_head_snapshot(&conn, &journal.old_head_snapshot)?;
    let id = journal.id.clone();
    archive_aborted(project_dir)?;
    Ok(RecoverOutcome::Aborted { id })
}

pub fn commit_metadata(project_dir: &Path, journal: &RestoreTransaction) -> Result<()> {
    let mut conn = history::ensure_initialized(project_dir)?;
    if journal.operation == "undo" {
        if let Some(event_id) = journal.undo_event_id {
            history::mark_undone(&conn, event_id)?;
        }
        history::set_head_snapshot(&conn, &journal.target_snapshot)?;
        return Ok(());
    }

    if history::event_for_transaction(&conn, &journal.transaction_id)?.is_none() {
        let before = load_snapshot(project_dir, &journal.old_head_snapshot)?;
        let after = load_snapshot(project_dir, &journal.target_snapshot)?;
        let diff = diff_snapshots(&before, &after);
        let timestamp = Utc::now().to_rfc3339();
        history::insert_event(
            &mut conn,
            history::NewEvent {
                kind: &journal.planned_event_kind,
                started_dirty: false,
                timestamp: &timestamp,
                command: &journal.planned_event_command,
                command_argv_json: None,
                command_cwd_relative: ".",
                exit_code: 0,
                before_snapshot: &journal.old_head_snapshot,
                after_snapshot: &journal.target_snapshot,
                diff: &diff,
                transaction_id: Some(&journal.transaction_id),
            },
        )?;
    }
    history::set_head_snapshot(&conn, &journal.target_snapshot)?;
    Ok(())
}

pub fn validate_journal_snapshots(project_dir: &Path, journal: &RestoreTransaction) -> Result<()> {
    load_snapshot(project_dir, &journal.old_head_snapshot).with_context(|| {
        format!(
            "old head snapshot {} is missing or invalid",
            journal.old_head_snapshot
        )
    })?;
    load_snapshot(project_dir, &journal.target_snapshot).with_context(|| {
        format!(
            "target snapshot {} is missing or invalid",
            journal.target_snapshot
        )
    })?;
    Ok(())
}

fn restore_whole_worktree(project_dir: &Path, target_snapshot_id: &str) -> Result<()> {
    let current = scan_worktree(project_dir)?;
    let target = load_snapshot(project_dir, target_snapshot_id)?;
    let plan = build_restore_plan(&current, &target)?;
    validate_restore_plan(project_dir, &plan)?;
    apply_restore_plan(project_dir, &target, &plan)
}

fn archive_active(project_dir: &Path, bucket: &str) -> Result<()> {
    let active = active_path(project_dir);
    if !active.exists() {
        return Ok(());
    }
    let journal = load_active(project_dir).ok();
    let id = journal
        .as_ref()
        .map(|journal| journal.id.as_str())
        .unwrap_or("unknown");
    let dir = project_dir.join(REWIND_DIR).join("journal").join(bucket);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(format!("{}.json", safe_archive_id(id)));
    fs::rename(&active, &target)
        .with_context(|| format!("archiving {} to {}", active.display(), target.display()))?;
    Ok(())
}

fn safe_archive_id(id: &str) -> String {
    let sanitized = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "unknown".to_owned()
    } else {
        sanitized.to_owned()
    }
}
