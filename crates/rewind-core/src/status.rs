use crate::config;
use crate::diff::{diff_snapshots, ChangeType, SnapshotDiff};
use crate::history;
use crate::snapshot::{load_snapshot, scan_worktree_with_ignored, SnapshotManifest};
use anyhow::{bail, Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub head_snapshot: String,
    pub diff: SnapshotDiff,
    pub ignored_paths: Vec<String>,
}

impl WorktreeStatus {
    pub fn is_clean(&self) -> bool {
        self.diff.changes.is_empty()
            && self.diff.added_dirs.is_empty()
            && self.diff.deleted_dirs.is_empty()
    }

    pub fn added_files(&self) -> Vec<&str> {
        self.files_by_type(ChangeType::Created)
    }

    pub fn modified_files(&self) -> Vec<&str> {
        self.files_by_type(ChangeType::Modified)
    }

    pub fn deleted_files(&self) -> Vec<&str> {
        self.files_by_type(ChangeType::Deleted)
    }

    fn files_by_type(&self, change_type: ChangeType) -> Vec<&str> {
        self.diff
            .changes
            .iter()
            .filter(|change| change.change_type == change_type)
            .map(|change| change.path.as_str())
            .collect()
    }
}

pub fn worktree_status(project_dir: &Path) -> Result<WorktreeStatus> {
    let conn = history::ensure_initialized(project_dir)?;
    let head_snapshot = history::get_head_snapshot(&conn)?
        .context("workspace has no head snapshot; run `rewind init` again")?;
    let head = load_snapshot(project_dir, &head_snapshot)?;
    compare_current_to_head(project_dir, &head_snapshot, &head)
}

pub fn compare_current_to_head(
    project_dir: &Path,
    head_snapshot: &str,
    head: &SnapshotManifest,
) -> Result<WorktreeStatus> {
    let scan = scan_worktree_with_ignored(project_dir)?;
    let head = effective_head(project_dir, head)?;
    Ok(WorktreeStatus {
        head_snapshot: head_snapshot.to_owned(),
        diff: diff_snapshots(&head, &scan.manifest),
        ignored_paths: scan.ignored_paths,
    })
}

pub fn require_clean(
    project_dir: &Path,
    head_snapshot: &str,
    head: &SnapshotManifest,
) -> Result<()> {
    let status = compare_current_to_head(project_dir, head_snapshot, head)?;
    if status.is_clean() {
        return Ok(());
    }

    bail!("{}", dirty_report(&status));
}

pub fn dirty_report(status: &WorktreeStatus) -> String {
    let mut report = format!(
        "Rewind worktree dirty.\nHead snapshot: {}\n",
        status.head_snapshot
    );
    append_group(&mut report, "Added", &status.added_files());
    append_group(&mut report, "Modified", &status.modified_files());
    append_group(&mut report, "Deleted", &status.deleted_files());
    append_group(&mut report, "Added directories", &status.diff.added_dirs);
    append_group(
        &mut report,
        "Deleted directories",
        &status.diff.deleted_dirs,
    );
    report
}

pub fn append_ignored_report(report: &mut String, status: &WorktreeStatus) {
    append_group(report, "Ignored", &status.ignored_paths);
}

fn effective_head(project_dir: &Path, head: &SnapshotManifest) -> Result<SnapshotManifest> {
    let Some(rules) = config::load_ignore_rules(project_dir)? else {
        return Ok(head.clone());
    };
    let directories = head
        .directories
        .iter()
        .filter(|path| !rules.is_ignored(path, true))
        .cloned()
        .collect();
    let files = head
        .files
        .iter()
        .filter(|(path, _)| !rules.is_ignored(path, false))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect();
    let symlinks = head
        .symlinks
        .iter()
        .filter(|(path, _)| !rules.is_ignored(path, false))
        .map(|(path, entry)| (path.clone(), entry.clone()))
        .collect();
    Ok(SnapshotManifest {
        manifest_version: head.manifest_version,
        id: head.id.clone(),
        created_at: head.created_at.clone(),
        directories,
        files,
        symlinks,
    })
}

fn append_group<T: AsRef<str>>(report: &mut String, title: &str, paths: &[T]) {
    if paths.is_empty() {
        return;
    }

    report.push('\n');
    report.push_str(title);
    report.push_str(":\n");
    for path in paths {
        report.push_str("  ");
        report.push_str(path.as_ref());
        report.push('\n');
    }
}
