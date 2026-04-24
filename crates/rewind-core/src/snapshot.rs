use crate::object_store::{hash_file, ObjectStore};
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub id: String,
    pub created_at: String,
    #[serde(default)]
    pub directories: BTreeSet<String>,
    pub files: BTreeMap<String, FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub hash: String,
    pub size: u64,
}

pub fn create_snapshot(project_dir: &Path) -> Result<SnapshotManifest> {
    scan_project(project_dir, ScanMode::StoreObjects)
}

pub fn scan_worktree(project_dir: &Path) -> Result<SnapshotManifest> {
    scan_project(project_dir, ScanMode::ReadOnly)
}

fn scan_project(project_dir: &Path, mode: ScanMode) -> Result<SnapshotManifest> {
    let rewind_dir = project_dir.join(REWIND_DIR);
    if !rewind_dir.is_dir() {
        bail!(
            "{} is not initialized; run `rewind init` first",
            project_dir.display()
        );
    }

    let object_store = ObjectStore::new(&rewind_dir);
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(project_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(should_descend)
    {
        let entry = entry.with_context(|| format!("walking {}", project_dir.display()))?;
        let path = entry.path();
        if path == project_dir {
            continue;
        }

        let relative = relative_path(project_dir, path)?;
        let file_type = entry.file_type();

        if file_type.is_dir() {
            directories.insert(relative);
        } else if file_type.is_file() {
            let (hash, size) = match mode {
                ScanMode::StoreObjects => object_store.store_file(path)?,
                ScanMode::ReadOnly => hash_file(path)?,
            };
            files.insert(relative, FileEntry { hash, size });
        }
    }

    let created_at = Utc::now().to_rfc3339();
    let id = snapshot_id(&directories, &files);
    Ok(SnapshotManifest {
        id,
        created_at,
        directories,
        files,
    })
}

#[derive(Debug, Clone, Copy)]
enum ScanMode {
    StoreObjects,
    ReadOnly,
}

pub fn write_snapshot(project_dir: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let snapshots_dir = project_dir.join(REWIND_DIR).join("snapshots");
    fs::create_dir_all(&snapshots_dir)
        .with_context(|| format!("creating {}", snapshots_dir.display()))?;

    let final_path = snapshot_path(project_dir, &manifest.id);
    let tmp_path = snapshots_dir.join(format!("{}.json.tmp", manifest.id));
    if final_path.exists() {
        return Ok(());
    }

    let bytes = serde_json::to_vec_pretty(manifest).context("serializing snapshot manifest")?;

    fs::write(&tmp_path, bytes).with_context(|| format!("writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "renaming {} to {}",
            tmp_path.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

pub fn load_snapshot(project_dir: &Path, snapshot_id: &str) -> Result<SnapshotManifest> {
    let path = snapshot_path(project_dir, snapshot_id);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

pub fn snapshot_path(project_dir: &Path, snapshot_id: &str) -> PathBuf {
    project_dir
        .join(REWIND_DIR)
        .join("snapshots")
        .join(format!("{snapshot_id}.json"))
}

fn should_descend(entry: &DirEntry) -> bool {
    entry.file_name() != REWIND_DIR
}

fn relative_path(project_dir: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(project_dir).with_context(|| {
        format!(
            "making {} relative to {}",
            path.display(),
            project_dir.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn snapshot_id(directories: &BTreeSet<String>, files: &BTreeMap<String, FileEntry>) -> String {
    let mut hasher = Sha256::new();
    for directory in directories {
        hasher.update(b"dir\0");
        hasher.update(directory.as_bytes());
        hasher.update(b"\0");
    }
    for (path, entry) in files {
        hasher.update(b"file\0");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.hash.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.size.to_string().as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
