use crate::diff::diff_snapshots;
use crate::history;
use crate::restore::{apply_restore_plan, build_restore_plan, validate_restore_plan, RestorePlan};
use crate::snapshot::{load_snapshot, SnapshotManifest};
use crate::status::{compare_current_to_head, WorktreeStatus};
use crate::transaction::{self, DebugStop, RestoreTransaction, TransactionPhase};
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

#[derive(Debug, Clone)]
pub enum CheckoutTarget {
    Checkpoint { name: String, snapshot_id: String },
    BeforeEvent { event_id: i64, snapshot_id: String },
    AfterEvent { event_id: i64, snapshot_id: String },
    Snapshot { snapshot_id: String },
}

impl CheckoutTarget {
    pub fn snapshot_id(&self) -> &str {
        match self {
            Self::Checkpoint { snapshot_id, .. }
            | Self::BeforeEvent { snapshot_id, .. }
            | Self::AfterEvent { snapshot_id, .. }
            | Self::Snapshot { snapshot_id } => snapshot_id,
        }
    }

    pub fn command(&self) -> String {
        match self {
            Self::Checkpoint { name, .. } => format!("checkout --checkpoint {name}"),
            Self::BeforeEvent { event_id, .. } => format!("checkout --before {event_id}"),
            Self::AfterEvent { event_id, .. } => format!("checkout --after {event_id}"),
            Self::Snapshot { snapshot_id } => format!("checkout --snapshot {snapshot_id}"),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Checkpoint { name, .. } => format!("checkpoint {name}"),
            Self::BeforeEvent { event_id, .. } => format!("before event {event_id}"),
            Self::AfterEvent { event_id, .. } => format!("after event {event_id}"),
            Self::Snapshot { snapshot_id } => format!("snapshot {snapshot_id}"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CheckoutOutcome {
    Applied { event_id: i64, plan: RestorePlan },
    DryRun { plan: RestorePlan },
    Dirty { status: WorktreeStatus },
    AlreadyAtTarget,
}

pub fn checkout(
    project_dir: &Path,
    target: CheckoutTarget,
    dry_run: bool,
) -> Result<CheckoutOutcome> {
    checkout_with_debug(project_dir, target, dry_run, DebugStop::None)
}

pub fn checkout_with_debug(
    project_dir: &Path,
    target: CheckoutTarget,
    dry_run: bool,
    debug_stop: DebugStop,
) -> Result<CheckoutOutcome> {
    let conn = history::ensure_initialized(project_dir)?;
    transaction::ensure_no_active(project_dir)?;
    let head_snapshot = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let head = load_snapshot(project_dir, &head_snapshot)?;
    let status = compare_current_to_head(project_dir, &head_snapshot, &head)?;
    if !status.is_clean() {
        return Ok(CheckoutOutcome::Dirty { status });
    }

    let target_snapshot_id = target.snapshot_id().to_owned();
    let target_snapshot = load_snapshot(project_dir, &target_snapshot_id)
        .with_context(|| format!("loading target snapshot {target_snapshot_id}"))?;
    if head_snapshot == target_snapshot_id {
        return Ok(CheckoutOutcome::AlreadyAtTarget);
    }

    let plan = build_restore_plan(&head, &target_snapshot)?;
    validate_restore_plan(project_dir, &plan)?;
    if dry_run {
        return Ok(CheckoutOutcome::DryRun { plan });
    }

    let mut journal = RestoreTransaction::new(
        "checkout",
        &target.command(),
        &head_snapshot,
        &target_snapshot.id,
        "checkout",
        &target.command(),
        plan.clone(),
    );
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterJournal {
        anyhow::bail!("debug stop after journal");
    }
    journal.phase = TransactionPhase::Applying;
    transaction::write_active(project_dir, &journal)?;
    apply_restore_plan(project_dir, &target_snapshot, &plan)?;
    if debug_stop == DebugStop::AfterApply {
        anyhow::bail!("debug stop after apply");
    }
    journal.phase = TransactionPhase::Committing;
    transaction::write_active(project_dir, &journal)?;
    let event_id = insert_checkout_event(
        conn,
        &head_snapshot,
        &head,
        &target_snapshot,
        &target,
        &journal.id,
    )?;
    journal.phase = TransactionPhase::Committed;
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterCommit {
        anyhow::bail!("debug stop after commit");
    }
    transaction::archive_completed(project_dir)?;
    Ok(CheckoutOutcome::Applied { event_id, plan })
}

fn insert_checkout_event(
    mut conn: rusqlite::Connection,
    before_snapshot_id: &str,
    before: &SnapshotManifest,
    after: &SnapshotManifest,
    target: &CheckoutTarget,
    transaction_id: &str,
) -> Result<i64> {
    let diff = diff_snapshots(before, after);
    let timestamp = Utc::now().to_rfc3339();
    let event_id = history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "checkout",
            started_dirty: false,
            timestamp: &timestamp,
            command: &target.command(),
            command_argv_json: None,
            command_cwd_relative: ".",
            exit_code: 0,
            before_snapshot: before_snapshot_id,
            after_snapshot: &after.id,
            diff: &diff,
            transaction_id: Some(transaction_id),
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;
    Ok(event_id)
}
