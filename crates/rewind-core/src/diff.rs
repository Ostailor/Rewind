use crate::snapshot::{FileEntry, SnapshotManifest};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeType {
    Created,
    Modified,
    Deleted,
}

impl ChangeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub change_type: ChangeType,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub changes: Vec<FileChange>,
    pub created_count: usize,
    pub modified_count: usize,
    pub deleted_count: usize,
    pub added_dirs: Vec<String>,
    pub deleted_dirs: Vec<String>,
}

pub fn diff_snapshots(before: &SnapshotManifest, after: &SnapshotManifest) -> SnapshotDiff {
    let paths = before
        .files
        .keys()
        .chain(after.files.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut diff = SnapshotDiff::default();
    for path in paths {
        match (before.files.get(&path), after.files.get(&path)) {
            (None, Some(after_entry)) => {
                diff.created_count += 1;
                diff.changes
                    .push(change(path, ChangeType::Created, None, Some(after_entry)));
            }
            (Some(before_entry), None) => {
                diff.deleted_count += 1;
                diff.changes
                    .push(change(path, ChangeType::Deleted, Some(before_entry), None));
            }
            (Some(before_entry), Some(after_entry)) if before_entry.hash != after_entry.hash => {
                diff.modified_count += 1;
                diff.changes.push(change(
                    path,
                    ChangeType::Modified,
                    Some(before_entry),
                    Some(after_entry),
                ));
            }
            _ => {}
        }
    }

    diff.added_dirs = after
        .directories
        .difference(&before.directories)
        .cloned()
        .collect();
    diff.deleted_dirs = before
        .directories
        .difference(&after.directories)
        .cloned()
        .collect();

    diff
}

fn change(
    path: String,
    change_type: ChangeType,
    before: Option<&FileEntry>,
    after: Option<&FileEntry>,
) -> FileChange {
    FileChange {
        path,
        change_type,
        before_hash: before.map(|entry| entry.hash.clone()),
        after_hash: after.map(|entry| entry.hash.clone()),
    }
}
