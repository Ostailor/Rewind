use crate::snapshot::{FileEntry, SnapshotManifest, SymlinkEntry};
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
    pub before_kind: Option<String>,
    pub after_kind: Option<String>,
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
    let paths = entry_paths(before)
        .into_iter()
        .chain(entry_paths(after))
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut diff = SnapshotDiff::default();
    for path in paths {
        match (entry_ref(before, &path), entry_ref(after, &path)) {
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
            (Some(before_entry), Some(after_entry)) if before_entry != after_entry => {
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
    before: Option<EntryRef<'_>>,
    after: Option<EntryRef<'_>>,
) -> FileChange {
    FileChange {
        path,
        change_type,
        before_hash: before.and_then(|entry| entry.hash()),
        after_hash: after.and_then(|entry| entry.hash()),
        before_kind: before.map(|entry| entry.kind().to_owned()),
        after_kind: after.map(|entry| entry.kind().to_owned()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryRef<'a> {
    Directory,
    File(&'a FileEntry),
    Symlink(&'a SymlinkEntry),
}

impl EntryRef<'_> {
    fn kind(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File(_) => "file",
            Self::Symlink(_) => "symlink",
        }
    }

    fn hash(self) -> Option<String> {
        match self {
            Self::File(entry) => Some(entry.hash.clone()),
            Self::Directory | Self::Symlink(_) => None,
        }
    }
}

fn entry_ref<'a>(snapshot: &'a SnapshotManifest, path: &str) -> Option<EntryRef<'a>> {
    if let Some(file) = snapshot.files.get(path) {
        Some(EntryRef::File(file))
    } else if let Some(symlink) = snapshot.symlinks.get(path) {
        Some(EntryRef::Symlink(symlink))
    } else if snapshot.directories.contains(path) {
        Some(EntryRef::Directory)
    } else {
        None
    }
}

fn entry_paths(snapshot: &SnapshotManifest) -> BTreeSet<&String> {
    snapshot
        .directories
        .iter()
        .chain(snapshot.files.keys())
        .chain(snapshot.symlinks.keys())
        .collect()
}
