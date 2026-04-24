use crate::history;
use crate::object_store::ObjectStore;
use crate::path_safety::{
    ensure_no_symlink_in_path, validate_relative_path, validate_snapshot_paths,
};
use crate::snapshot::{compute_snapshot_id, write_snapshot};
use crate::snapshot::{load_snapshot, FileEntry, SnapshotManifest};
use crate::status::{compare_current_to_head, WorktreeStatus};
use crate::transaction::{self, DebugStop, RestoreTransaction, TransactionPhase};
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlan {
    pub create_dirs: Vec<PathBuf>,
    pub remove_dirs: Vec<PathBuf>,
    pub write_files: Vec<PathBuf>,
    pub remove_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum UndoOutcome {
    Applied { event_id: i64 },
    DryRun { event_id: i64, plan: RestorePlan },
    Dirty { status: WorktreeStatus },
    NothingToUndo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreSource {
    Before,
    After,
}

impl RestoreSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

#[derive(Debug, Clone)]
pub enum TargetedRestoreOutcome {
    Applied { event_id: i64, plan: RestorePlan },
    DryRun { plan: RestorePlan },
    Dirty { status: WorktreeStatus },
    NothingToRestore,
}

pub fn undo_latest(project_dir: &Path, dry_run: bool) -> Result<UndoOutcome> {
    undo_latest_with_debug(project_dir, dry_run, DebugStop::None)
}

pub fn undo_latest_with_debug(
    project_dir: &Path,
    dry_run: bool,
    debug_stop: DebugStop,
) -> Result<UndoOutcome> {
    let conn = history::ensure_initialized(project_dir)?;
    transaction::ensure_no_active(project_dir)?;
    let Some(head_snapshot) = history::get_head_snapshot(&conn)? else {
        return Ok(UndoOutcome::NothingToUndo);
    };
    let head = load_snapshot(project_dir, &head_snapshot)?;

    let status = compare_current_to_head(project_dir, &head_snapshot, &head)?;
    if !status.is_clean() {
        return Ok(UndoOutcome::Dirty { status });
    }

    let Some(event) = history::latest_non_undone_event_for_head(&conn, &head_snapshot)? else {
        return Ok(UndoOutcome::NothingToUndo);
    };

    let target = load_snapshot(project_dir, &event.before_snapshot)?;
    let plan = build_restore_plan(&head, &target)?;
    validate_restore_plan(project_dir, &plan)?;

    if dry_run {
        return Ok(UndoOutcome::DryRun {
            event_id: event.id,
            plan,
        });
    }

    let mut journal = RestoreTransaction::new(
        "undo",
        "undo",
        &head_snapshot,
        &event.before_snapshot,
        "undo",
        "undo",
        plan.clone(),
    );
    journal.undo_event_id = Some(event.id);
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterJournal {
        bail!("debug stop after journal");
    }
    journal.phase = TransactionPhase::Applying;
    transaction::write_active(project_dir, &journal)?;
    apply_restore_plan(project_dir, &target, &plan)?;
    if debug_stop == DebugStop::AfterApply {
        bail!("debug stop after apply");
    }
    journal.phase = TransactionPhase::Committing;
    transaction::write_active(project_dir, &journal)?;
    history::mark_undone(&conn, event.id)?;
    history::set_head_snapshot(&conn, &event.before_snapshot)?;
    journal.phase = TransactionPhase::Committed;
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterCommit {
        bail!("debug stop after commit");
    }
    transaction::archive_completed(project_dir)?;
    Ok(UndoOutcome::Applied { event_id: event.id })
}

pub fn build_restore_plan(
    current: &SnapshotManifest,
    target: &SnapshotManifest,
) -> Result<RestorePlan> {
    validate_snapshot_paths(current.directories.iter(), current.files.keys())?;
    validate_snapshot_paths(target.directories.iter(), target.files.keys())?;

    let current_dirs = directories_with_file_parents(current);
    let target_dirs = directories_with_file_parents(target);

    let remove_files = current
        .files
        .keys()
        .filter(|path| !target.files.contains_key(*path))
        .map(|path| validate_relative_path(path))
        .collect::<Result<Vec<_>>>()?;

    let write_files = target
        .files
        .iter()
        .filter(|(path, target_entry)| {
            current
                .files
                .get(*path)
                .is_none_or(|current_entry| current_entry.hash != target_entry.hash)
        })
        .map(|(path, _)| validate_relative_path(path))
        .collect::<Result<Vec<_>>>()?;

    let create_dirs = target_dirs
        .difference(&current_dirs)
        .map(|path| validate_relative_path(path))
        .collect::<Result<Vec<_>>>()?;

    let mut remove_dirs = current_dirs
        .difference(&target_dirs)
        .map(|path| validate_relative_path(path))
        .collect::<Result<Vec<_>>>()?;
    remove_dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    Ok(RestorePlan {
        create_dirs,
        remove_dirs,
        write_files,
        remove_files,
    })
}

pub fn validate_restore_plan(project_dir: &Path, plan: &RestorePlan) -> Result<()> {
    for path in plan
        .create_dirs
        .iter()
        .chain(plan.remove_dirs.iter())
        .chain(plan.write_files.iter())
        .chain(plan.remove_files.iter())
    {
        validate_relative_path(&path.to_string_lossy())?;
        ensure_no_symlink_in_path(project_dir, path)?;
    }
    Ok(())
}

pub fn targeted_restore(
    project_dir: &Path,
    path: &str,
    source: RestoreSource,
    event_id: i64,
    dry_run: bool,
) -> Result<TargetedRestoreOutcome> {
    targeted_restore_with_debug(
        project_dir,
        path,
        source,
        event_id,
        dry_run,
        DebugStop::None,
    )
}

pub fn targeted_restore_with_debug(
    project_dir: &Path,
    path: &str,
    source: RestoreSource,
    event_id: i64,
    dry_run: bool,
    debug_stop: DebugStop,
) -> Result<TargetedRestoreOutcome> {
    let path = validate_relative_path(path)?;
    let conn = history::ensure_initialized(project_dir)?;
    transaction::ensure_no_active(project_dir)?;
    let Some(head_snapshot) = history::get_head_snapshot(&conn)? else {
        return Ok(TargetedRestoreOutcome::NothingToRestore);
    };
    let head = load_snapshot(project_dir, &head_snapshot)?;
    let status = compare_current_to_head(project_dir, &head_snapshot, &head)?;
    if !status.is_clean() {
        return Ok(TargetedRestoreOutcome::Dirty { status });
    }

    let event = history::get_event(&conn, event_id)?
        .with_context(|| format!("event {event_id} not found"))?;
    let source_snapshot_id = match source {
        RestoreSource::Before => &event.before_snapshot,
        RestoreSource::After => &event.after_snapshot,
    };
    let source_snapshot = load_snapshot(project_dir, source_snapshot_id)?;
    let plan = build_path_restore_plan(&head, &source_snapshot, &path)?;
    validate_restore_plan(project_dir, &plan)?;

    if plan.is_empty() {
        return Ok(TargetedRestoreOutcome::NothingToRestore);
    }

    if dry_run {
        return Ok(TargetedRestoreOutcome::DryRun { plan });
    }

    let after = build_path_restored_snapshot(&head, &source_snapshot, &path);
    write_snapshot(project_dir, &after)?;
    let diff = crate::diff::diff_snapshots(&head, &after);
    let command = format!(
        "restore {} --{} {}",
        path.to_string_lossy().replace('\\', "/"),
        source.as_str(),
        event_id
    );
    let mut journal = RestoreTransaction::new(
        "restore",
        &command,
        &head_snapshot,
        &after.id,
        "restore",
        &command,
        plan.clone(),
    );
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterJournal {
        bail!("debug stop after journal");
    }
    journal.phase = TransactionPhase::Applying;
    transaction::write_active(project_dir, &journal)?;
    apply_restore_plan(project_dir, &after, &plan)?;
    if debug_stop == DebugStop::AfterApply {
        bail!("debug stop after apply");
    }
    journal.phase = TransactionPhase::Committing;
    transaction::write_active(project_dir, &journal)?;
    let mut conn = conn;
    let timestamp = Utc::now().to_rfc3339();
    let restore_event_id = history::insert_event(
        &mut conn,
        history::NewEvent {
            kind: "restore",
            started_dirty: false,
            timestamp: &timestamp,
            command: &command,
            exit_code: 0,
            before_snapshot: &head_snapshot,
            after_snapshot: &after.id,
            diff: &diff,
            transaction_id: Some(&journal.id),
        },
    )?;
    history::set_head_snapshot(&conn, &after.id)?;
    journal.phase = TransactionPhase::Committed;
    transaction::write_active(project_dir, &journal)?;
    if debug_stop == DebugStop::AfterCommit {
        bail!("debug stop after commit");
    }
    transaction::archive_completed(project_dir)?;
    Ok(TargetedRestoreOutcome::Applied {
        event_id: restore_event_id,
        plan,
    })
}

pub fn build_path_restored_snapshot(
    current: &SnapshotManifest,
    source: &SnapshotManifest,
    path: &Path,
) -> SnapshotManifest {
    let path_string = path.to_string_lossy().replace('\\', "/");
    let mut directories = remove_subtree_dirs(&current.directories, &path_string);
    let mut files = remove_subtree_files(&current.files, &path_string);

    if source.directories.contains(&path_string) {
        directories.insert(path_string.clone());
    }
    for directory in &source.directories {
        if is_descendant(&path_string, directory) {
            directories.insert(directory.clone());
        }
    }
    if let Some(file) = source.files.get(&path_string) {
        files.insert(path_string.clone(), file.clone());
    }
    for (file_path, file) in &source.files {
        if is_descendant(&path_string, file_path) {
            files.insert(file_path.clone(), file.clone());
        }
    }

    let id = compute_snapshot_id(&directories, &files);
    SnapshotManifest {
        id,
        created_at: Utc::now().to_rfc3339(),
        directories,
        files,
    }
}

fn remove_subtree_dirs(directories: &BTreeSet<String>, root: &str) -> BTreeSet<String> {
    directories
        .iter()
        .filter(|path| path.as_str() != root && !is_descendant(root, path))
        .cloned()
        .collect()
}

fn remove_subtree_files(
    files: &BTreeMap<String, FileEntry>,
    root: &str,
) -> BTreeMap<String, FileEntry> {
    files
        .iter()
        .filter(|(path, _)| path.as_str() != root && !is_descendant(root, path))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect()
}

pub fn build_path_restore_plan(
    current: &SnapshotManifest,
    source: &SnapshotManifest,
    path: &Path,
) -> Result<RestorePlan> {
    validate_snapshot_paths(current.directories.iter(), current.files.keys())?;
    validate_snapshot_paths(source.directories.iter(), source.files.keys())?;
    let path_string = path.to_string_lossy().replace('\\', "/");

    let current_subset = snapshot_subset(current, &path_string);
    let source_subset = snapshot_subset(source, &path_string);
    build_restore_plan(&current_subset, &source_subset)
}

fn snapshot_subset(snapshot: &SnapshotManifest, root: &str) -> SnapshotManifest {
    let mut subset = SnapshotManifest {
        id: String::new(),
        created_at: snapshot.created_at.clone(),
        directories: BTreeSet::new(),
        files: std::collections::BTreeMap::new(),
    };

    if snapshot.directories.contains(root) {
        subset.directories.insert(root.to_owned());
    }
    for directory in &snapshot.directories {
        if is_descendant(root, directory) {
            subset.directories.insert(directory.clone());
        }
    }
    if let Some(file) = snapshot.files.get(root) {
        subset.files.insert(root.to_owned(), file.clone());
    }
    for (path, file) in &snapshot.files {
        if is_descendant(root, path) {
            subset.files.insert(path.clone(), file.clone());
        }
    }

    subset
}

fn is_descendant(root: &str, path: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

impl RestorePlan {
    pub fn is_empty(&self) -> bool {
        self.create_dirs.is_empty()
            && self.remove_dirs.is_empty()
            && self.write_files.is_empty()
            && self.remove_files.is_empty()
    }
}

pub fn apply_restore_plan(
    project_dir: &Path,
    target: &SnapshotManifest,
    plan: &RestorePlan,
) -> Result<()> {
    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));

    for path in &plan.remove_files {
        let full_path = project_dir.join(path);
        if full_path.exists() {
            fs::remove_file(&full_path)
                .with_context(|| format!("removing file {}", full_path.display()))?;
        }
    }

    for path in &plan.create_dirs {
        let full_path = project_dir.join(path);
        fs::create_dir_all(&full_path)
            .with_context(|| format!("creating directory {}", full_path.display()))?;
    }

    for path in &plan.write_files {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(project_dir.join(parent))
                    .with_context(|| format!("creating parent directory {}", parent.display()))?;
            }
        }

        let path_key = path.to_string_lossy().replace('\\', "/");
        let entry = target
            .files
            .get(&path_key)
            .with_context(|| format!("missing target manifest entry for {}", path.display()))?;
        let object_path = object_store.object_path(&entry.hash);
        let destination = project_dir.join(path);
        fs::copy(&object_path, &destination).with_context(|| {
            format!(
                "restoring {} from object {}",
                destination.display(),
                object_path.display()
            )
        })?;
    }

    for path in &plan.remove_dirs {
        let full_path = project_dir.join(path);
        if !full_path.exists() {
            continue;
        }
        if full_path.read_dir()?.next().is_some() {
            bail!(
                "refusing to remove non-empty directory {}",
                full_path.display()
            );
        }
        fs::remove_dir(&full_path)
            .with_context(|| format!("removing directory {}", full_path.display()))?;
    }

    Ok(())
}

fn directories_with_file_parents(snapshot: &SnapshotManifest) -> BTreeSet<String> {
    let mut directories = snapshot.directories.clone();
    for path in snapshot.files.keys() {
        let mut parent = Path::new(path).parent();
        while let Some(directory) = parent {
            if directory.as_os_str().is_empty() {
                break;
            }
            directories.insert(directory.to_string_lossy().replace('\\', "/"));
            parent = directory.parent();
        }
    }
    directories
}
