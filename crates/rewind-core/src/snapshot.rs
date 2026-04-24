use crate::ignore::IgnoreRules;
use crate::object_store::{hash_file, ObjectStore};
use crate::REWIND_DIR;
use crate::{config, history};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

pub const CURRENT_SNAPSHOT_MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    #[serde(default = "default_manifest_version")]
    pub manifest_version: u32,
    pub id: String,
    pub created_at: String,
    #[serde(default)]
    pub directories: BTreeSet<String>,
    pub files: BTreeMap<String, FileEntry>,
    #[serde(default)]
    pub symlinks: BTreeMap<String, SymlinkEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub hash: String,
    pub size: u64,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymlinkEntry {
    pub target: String,
}

pub fn create_snapshot(project_dir: &Path) -> Result<SnapshotManifest> {
    let mut snapshot = scan_project(project_dir, ScanMode::StoreObjects)?.manifest;
    carry_forward_ignored_entries(project_dir, &mut snapshot)?;
    snapshot.id = compute_snapshot_id_for_manifest(&snapshot);
    Ok(snapshot)
}

pub fn scan_worktree(project_dir: &Path) -> Result<SnapshotManifest> {
    Ok(scan_project(project_dir, ScanMode::ReadOnly)?.manifest)
}

#[derive(Debug, Clone)]
pub struct WorktreeScan {
    pub manifest: SnapshotManifest,
    pub ignored_paths: Vec<String>,
}

pub fn scan_worktree_with_ignored(project_dir: &Path) -> Result<WorktreeScan> {
    scan_project(project_dir, ScanMode::ReadOnly)
}

pub fn scan_plain_directory(root: &Path) -> Result<SnapshotManifest> {
    Ok(scan_directory(root, ScanMode::ReadOnly, false, None)?.manifest)
}

fn scan_project(project_dir: &Path, mode: ScanMode) -> Result<WorktreeScan> {
    let rules = config::load_ignore_rules(project_dir)?;
    scan_directory(project_dir, mode, true, rules.as_ref())
}

fn scan_directory(
    project_dir: &Path,
    mode: ScanMode,
    require_rewind: bool,
    ignore_rules: Option<&IgnoreRules>,
) -> Result<WorktreeScan> {
    let rewind_dir = project_dir.join(REWIND_DIR);
    if require_rewind && !rewind_dir.is_dir() {
        bail!(
            "{} is not initialized; run `rewind init` first",
            project_dir.display()
        );
    }

    let object_store = ObjectStore::new(&rewind_dir);
    let mut directories = BTreeSet::new();
    let mut files = BTreeMap::new();
    let mut symlinks = BTreeMap::new();
    let mut ignored_paths = Vec::new();

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
        if let Some(rules) = ignore_rules {
            if rules.is_ignored(&relative, file_type.is_dir()) {
                ignored_paths.push(relative);
                continue;
            }
        }

        if file_type.is_symlink() {
            let target = fs::read_link(path)
                .with_context(|| format!("reading symlink target {}", path.display()))?;
            symlinks.insert(
                relative,
                SymlinkEntry {
                    target: target.to_string_lossy().into_owned(),
                },
            );
        } else if file_type.is_dir() {
            directories.insert(relative);
        } else if file_type.is_file() {
            let (hash, size) = match mode {
                ScanMode::StoreObjects => object_store.store_file(path)?,
                ScanMode::ReadOnly => hash_file(path)?,
            };
            files.insert(
                relative,
                FileEntry {
                    hash,
                    size,
                    executable: is_executable(path)?,
                },
            );
        } else {
            bail!("unsupported filesystem object: {}", path.display());
        }
    }

    let created_at = Utc::now().to_rfc3339();
    let id = compute_snapshot_id_v2(&directories, &files, &symlinks);
    Ok(WorktreeScan {
        manifest: SnapshotManifest {
            manifest_version: CURRENT_SNAPSHOT_MANIFEST_VERSION,
            id,
            created_at,
            directories,
            files,
            symlinks,
        },
        ignored_paths,
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

fn carry_forward_ignored_entries(
    project_dir: &Path,
    snapshot: &mut SnapshotManifest,
) -> Result<()> {
    let Some(rules) = config::load_ignore_rules(project_dir)? else {
        return Ok(());
    };
    let conn = history::ensure_initialized(project_dir)?;
    let Some(head_snapshot) = history::get_head_snapshot(&conn)? else {
        return Ok(());
    };
    let head = load_snapshot(project_dir, &head_snapshot)?;

    // Ignored tracked paths stay represented by the previous head snapshot.
    // This prevents adding an ignore rule from creating fake deletions in the
    // next unrelated event while still hiding current ignored worktree noise.
    for directory in &head.directories {
        if rules.is_ignored(directory, true) {
            snapshot.directories.insert(directory.clone());
        }
    }
    for (path, entry) in &head.files {
        if rules.is_ignored(path, false) {
            insert_parent_directories(path, &mut snapshot.directories);
            snapshot.files.insert(path.clone(), entry.clone());
        }
    }
    for (path, entry) in &head.symlinks {
        if rules.is_ignored(path, false) {
            insert_parent_directories(path, &mut snapshot.directories);
            snapshot.symlinks.insert(path.clone(), entry.clone());
        }
    }
    Ok(())
}

fn insert_parent_directories(path: &str, directories: &mut BTreeSet<String>) {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    let mut current = String::new();
    for part in parts {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        directories.insert(current.clone());
    }
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

pub fn compute_snapshot_id(
    directories: &BTreeSet<String>,
    files: &BTreeMap<String, FileEntry>,
) -> String {
    compute_snapshot_id_v1(directories, files)
}

pub fn compute_snapshot_id_for_manifest(snapshot: &SnapshotManifest) -> String {
    if snapshot.manifest_version <= 1 {
        compute_snapshot_id_v1(&snapshot.directories, &snapshot.files)
    } else {
        compute_snapshot_id_v2(&snapshot.directories, &snapshot.files, &snapshot.symlinks)
    }
}

fn compute_snapshot_id_v1(
    directories: &BTreeSet<String>,
    files: &BTreeMap<String, FileEntry>,
) -> String {
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

pub fn compute_snapshot_id_v2(
    directories: &BTreeSet<String>,
    files: &BTreeMap<String, FileEntry>,
    symlinks: &BTreeMap<String, SymlinkEntry>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"manifest-version\0");
    hasher.update(CURRENT_SNAPSHOT_MANIFEST_VERSION.to_string().as_bytes());
    hasher.update(b"\0");
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
        hasher.update(if entry.executable {
            b"exec".as_slice()
        } else {
            b"noexec".as_slice()
        });
        hasher.update(b"\0");
    }
    for (path, entry) in symlinks {
        hasher.update(b"symlink\0");
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(entry.target.as_bytes());
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn default_manifest_version() -> u32 {
    1
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool> {
    Ok(false)
}
