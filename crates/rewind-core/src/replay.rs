use crate::history::{self, Event};
use crate::object_store::ObjectStore;
use crate::path_safety::validate_relative_path;
use crate::snapshot::{load_snapshot, scan_plain_directory, SnapshotManifest};
use crate::transaction;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_TEXT_DIFF_BYTES: u64 = 1024 * 1024;
const MAX_TEXT_DIFFS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayMode {
    DryRun,
    Sandbox,
    Compare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaySource {
    Argv(Vec<String>),
    LegacyShellFallback(String),
}

impl ReplaySource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Argv(_) => "argv",
            Self::LegacyShellFallback(_) => "legacy-shell-fallback",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplayPlan {
    pub event_id: i64,
    pub command: String,
    pub source: ReplaySource,
    pub working_dir: String,
    pub before_snapshot: String,
    pub after_snapshot: String,
    pub original_exit_code: i32,
    pub keep_sandbox: bool,
    pub detailed_compare: bool,
    pub active_journal_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReplayArtifacts {
    pub sandbox_root: PathBuf,
    pub workspace_root: PathBuf,
    pub home_dir: PathBuf,
    pub tmp_dir: PathBuf,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub plan: ReplayPlan,
    pub exit_code: i32,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub comparison: ReplayComparison,
    pub artifacts: Option<ReplayArtifacts>,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayComparison {
    pub exact_match: bool,
    pub exit_code_match: bool,
    pub filesystem_match: bool,
    pub original_tree_id: String,
    pub replay_tree_id: String,
    pub only_in_original: Vec<String>,
    pub only_in_replay: Vec<String>,
    pub content_mismatches: Vec<String>,
    pub kind_mismatches: Vec<String>,
    pub text_diffs: Vec<TextDiffPreview>,
}

#[derive(Debug, Clone)]
pub struct TextDiffPreview {
    pub path: String,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReplayStats {
    pub run_events: usize,
    pub exact_argv: usize,
    pub legacy_fallback: usize,
    pub unsupported: usize,
}

pub fn plan(
    project_dir: &Path,
    event_id: i64,
    keep_sandbox: bool,
    detailed_compare: bool,
) -> Result<ReplayPlan> {
    let conn = history::ensure_initialized(project_dir)?;
    let event = history::get_event(&conn, event_id)?
        .with_context(|| format!("event {event_id} not found"))?;
    build_plan(project_dir, event, keep_sandbox, detailed_compare)
}

pub fn replay(
    project_dir: &Path,
    event_id: i64,
    mode: ReplayMode,
    keep_sandbox: bool,
) -> Result<ReplayOutcome> {
    let detailed_compare = mode == ReplayMode::Compare;
    let plan = plan(project_dir, event_id, keep_sandbox, detailed_compare)?;
    if mode == ReplayMode::DryRun {
        bail!("dry-run replay should use plan()");
    }

    let before = load_snapshot(project_dir, &plan.before_snapshot)?;
    let after = load_snapshot(project_dir, &plan.after_snapshot)?;
    validate_replay_cwd(&plan.working_dir)?;
    ensure_cwd_exists_in_snapshot(&before, &plan.working_dir)?;

    let artifacts = create_artifacts(plan.event_id)?;
    let cleanup = SandboxCleanup::new(artifacts.sandbox_root.clone(), keep_sandbox);
    materialize_snapshot(project_dir, &artifacts.workspace_root, &before)?;
    fs::create_dir_all(&artifacts.home_dir)?;
    fs::create_dir_all(&artifacts.tmp_dir)?;

    let cwd = if plan.working_dir == "." {
        artifacts.workspace_root.clone()
    } else {
        artifacts.workspace_root.join(&plan.working_dir)
    };
    let stdout = File::create(&artifacts.stdout_path)
        .with_context(|| format!("creating {}", artifacts.stdout_path.display()))?;
    let stderr = File::create(&artifacts.stderr_path)
        .with_context(|| format!("creating {}", artifacts.stderr_path.display()))?;
    let status = command_for_source(&plan.source)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .env_clear()
        .env("REWIND_REPLAY", "1")
        .env("HOME", &artifacts.home_dir)
        .env("TMPDIR", &artifacts.tmp_dir)
        .env("TMP", &artifacts.tmp_dir)
        .env("TEMP", &artifacts.tmp_dir)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .status()
        .with_context(|| format!("replaying event {}", plan.event_id))?;
    let exit_code = status.code().unwrap_or(1);

    let replay_snapshot = scan_plain_directory(&artifacts.workspace_root)?;
    let comparison = compare_snapshots(
        project_dir,
        &after,
        &replay_snapshot,
        &artifacts.workspace_root,
        plan.original_exit_code,
        exit_code,
    )?;
    let stdout_bytes = fs::metadata(&artifacts.stdout_path)?.len();
    let stderr_bytes = fs::metadata(&artifacts.stderr_path)?.len();
    let kept_artifacts = if keep_sandbox { Some(artifacts) } else { None };
    cleanup.finish()?;

    Ok(ReplayOutcome {
        plan,
        exit_code,
        stdout_bytes,
        stderr_bytes,
        comparison,
        artifacts: kept_artifacts,
    })
}

pub fn replay_stats(project_dir: &Path) -> Result<ReplayStats> {
    let conn = history::ensure_initialized(project_dir)?;
    let events = history::list_events(&conn)?;
    let mut stats = ReplayStats::default();
    for event in events.iter().filter(|event| event.kind == "run") {
        stats.run_events += 1;
        if let Some(argv) = parse_argv(event)? {
            if validate_replay_argv(&argv).is_ok() {
                stats.exact_argv += 1;
            } else {
                stats.unsupported += 1;
            }
        } else if cfg!(unix) {
            stats.legacy_fallback += 1;
        } else {
            stats.unsupported += 1;
        }
    }
    Ok(stats)
}

pub fn validate_replay_cwd(path: &str) -> Result<()> {
    if path == "." {
        return Ok(());
    }
    validate_relative_path(path)?;
    Ok(())
}

pub fn parse_argv(event: &Event) -> Result<Option<Vec<String>>> {
    let Some(json) = &event.command_argv_json else {
        return Ok(None);
    };
    let argv = serde_json::from_str::<Vec<String>>(json)
        .with_context(|| format!("event {} has malformed command_argv_json", event.id))?;
    if argv.is_empty() {
        bail!("event {} has empty command_argv_json", event.id);
    }
    Ok(Some(argv))
}

fn validate_replay_argv(argv: &[String]) -> Result<()> {
    let executable = argv
        .first()
        .with_context(|| "replay argv is unexpectedly empty")?;
    if executable.is_empty() {
        bail!("replay argv executable is empty");
    }

    let executable_path = Path::new(executable);
    if executable_path.is_absolute() {
        bail!(
            "replay refuses absolute executable path {executable}; exact argv replay must use PATH or workspace-relative executables"
        );
    }
    if executable_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("replay refuses executable paths containing ..");
    }

    Ok(())
}

fn build_plan(
    project_dir: &Path,
    event: Event,
    keep_sandbox: bool,
    detailed_compare: bool,
) -> Result<ReplayPlan> {
    if event.kind != "run" {
        bail!(
            "event {} has kind {}; replay only supports run events in v0.12",
            event.id,
            event.kind
        );
    }
    validate_replay_cwd(&event.command_cwd_relative)
        .with_context(|| format!("event {} has invalid command_cwd_relative", event.id))?;
    let source = if let Some(argv) = parse_argv(&event)? {
        validate_replay_argv(&argv)
            .with_context(|| format!("event {} is not safely replayable from argv", event.id))?;
        ReplaySource::Argv(argv)
    } else if cfg!(unix) {
        ReplaySource::LegacyShellFallback(event.command.clone())
    } else {
        bail!(
            "event {} has no exact argv; legacy shell fallback is unsupported on this platform",
            event.id
        );
    };
    let active_journal_warning = transaction::has_active(project_dir).then(|| {
        "Warning: active recovery transaction present. Replay uses stored snapshots only and does not modify the real workspace. Run rewind recover --status for workspace recovery status.".to_owned()
    });

    Ok(ReplayPlan {
        event_id: event.id,
        command: event.command,
        source,
        working_dir: event.command_cwd_relative,
        before_snapshot: event.before_snapshot,
        after_snapshot: event.after_snapshot,
        original_exit_code: event.exit_code,
        keep_sandbox,
        detailed_compare,
        active_journal_warning,
    })
}

fn create_artifacts(event_id: i64) -> Result<ReplayArtifacts> {
    let sandbox_root = std::env::temp_dir().join(format!(
        "rewind-replay-{event_id}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let workspace_root = sandbox_root.join("workspace");
    let home_dir = sandbox_root.join("home");
    let tmp_dir = sandbox_root.join("tmp");
    fs::create_dir_all(&workspace_root)
        .with_context(|| format!("creating {}", workspace_root.display()))?;
    Ok(ReplayArtifacts {
        stdout_path: sandbox_root.join("stdout.txt"),
        stderr_path: sandbox_root.join("stderr.txt"),
        sandbox_root,
        workspace_root,
        home_dir,
        tmp_dir,
    })
}

struct SandboxCleanup {
    root: PathBuf,
    keep: bool,
    finished: bool,
}

impl SandboxCleanup {
    fn new(root: PathBuf, keep: bool) -> Self {
        Self {
            root,
            keep,
            finished: false,
        }
    }

    fn finish(mut self) -> Result<()> {
        if !self.keep {
            fs::remove_dir_all(&self.root)
                .with_context(|| format!("removing sandbox {}", self.root.display()))?;
        }
        self.finished = true;
        Ok(())
    }
}

impl Drop for SandboxCleanup {
    fn drop(&mut self) {
        if !self.keep && !self.finished {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn materialize_snapshot(
    project_dir: &Path,
    workspace_root: &Path,
    snapshot: &SnapshotManifest,
) -> Result<()> {
    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    for directory in &snapshot.directories {
        fs::create_dir_all(workspace_root.join(directory))
            .with_context(|| format!("creating replay directory {directory}"))?;
    }
    for (path, entry) in &snapshot.files {
        let destination = workspace_root.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating replay parent {}", parent.display()))?;
        }
        fs::copy(object_store.object_path(&entry.hash), &destination)
            .with_context(|| format!("materializing replay file {path}"))?;
        set_executable(&destination, entry.executable)?;
    }
    for (path, entry) in &snapshot.symlinks {
        let destination = workspace_root.join(path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating replay parent {}", parent.display()))?;
        }
        create_symlink(&entry.target, &destination)?;
    }
    Ok(())
}

fn command_for_source(source: &ReplaySource) -> Command {
    match source {
        ReplaySource::Argv(argv) => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        }
        ReplaySource::LegacyShellFallback(command_string) => {
            let mut command = Command::new("sh");
            command.arg("-lc").arg(command_string);
            command
        }
    }
}

fn ensure_cwd_exists_in_snapshot(snapshot: &SnapshotManifest, cwd: &str) -> Result<()> {
    if cwd == "." || snapshot.directories.contains(cwd) {
        return Ok(());
    }
    bail!("replay working directory {cwd} does not exist in before snapshot")
}

fn compare_snapshots(
    project_dir: &Path,
    original: &SnapshotManifest,
    replay: &SnapshotManifest,
    replay_workspace: &Path,
    original_exit_code: i32,
    replay_exit_code: i32,
) -> Result<ReplayComparison> {
    let original_paths = all_paths(original);
    let replay_paths = all_paths(replay);
    let only_in_original = original_paths
        .difference(&replay_paths)
        .cloned()
        .collect::<Vec<_>>();
    let only_in_replay = replay_paths
        .difference(&original_paths)
        .cloned()
        .collect::<Vec<_>>();
    let mut content_mismatches = Vec::new();
    let mut kind_mismatches = Vec::new();
    for path in original_paths.intersection(&replay_paths) {
        match (entry_kind(original, path), entry_kind(replay, path)) {
            (Some("file"), Some("file")) if original.files.get(path) != replay.files.get(path) => {
                content_mismatches.push(path.clone());
            }
            (Some("symlink"), Some("symlink"))
                if original.symlinks.get(path) != replay.symlinks.get(path) =>
            {
                content_mismatches.push(path.clone());
            }
            (left, right) if left != right => kind_mismatches.push(path.clone()),
            _ => {}
        }
    }

    let filesystem_match = only_in_original.is_empty()
        && only_in_replay.is_empty()
        && content_mismatches.is_empty()
        && kind_mismatches.is_empty();
    let exit_code_match = original_exit_code == replay_exit_code;
    let mut comparison = ReplayComparison {
        exact_match: filesystem_match && exit_code_match && original.id == replay.id,
        exit_code_match,
        filesystem_match,
        original_tree_id: original.id.clone(),
        replay_tree_id: replay.id.clone(),
        only_in_original,
        only_in_replay,
        content_mismatches,
        kind_mismatches,
        text_diffs: Vec::new(),
    };
    comparison.text_diffs = text_diff_previews(
        project_dir,
        original,
        replay,
        replay_workspace,
        &comparison.content_mismatches,
    )?;
    Ok(comparison)
}

fn all_paths(snapshot: &SnapshotManifest) -> BTreeSet<String> {
    snapshot
        .directories
        .iter()
        .cloned()
        .chain(snapshot.files.keys().cloned())
        .chain(snapshot.symlinks.keys().cloned())
        .collect()
}

fn text_diff_previews(
    project_dir: &Path,
    original: &SnapshotManifest,
    replay: &SnapshotManifest,
    replay_workspace: &Path,
    paths: &[String],
) -> Result<Vec<TextDiffPreview>> {
    let object_store = ObjectStore::new(&project_dir.join(REWIND_DIR));
    let mut previews = Vec::new();
    for path in paths.iter().take(MAX_TEXT_DIFFS) {
        let Some(original_file) = original.files.get(path) else {
            continue;
        };
        let Some(replay_file) = replay.files.get(path) else {
            continue;
        };
        if original_file.size > MAX_TEXT_DIFF_BYTES || replay_file.size > MAX_TEXT_DIFF_BYTES {
            continue;
        }
        let original_bytes = fs::read(object_store.object_path(&original_file.hash))?;
        let replay_bytes = fs::read(replay_workspace.join(path))?;
        let (Ok(original_text), Ok(replay_text)) = (
            String::from_utf8(original_bytes),
            String::from_utf8(replay_bytes),
        ) else {
            continue;
        };
        previews.push(TextDiffPreview {
            path: path.clone(),
            lines: simple_text_diff(&original_text, &replay_text),
        });
    }
    Ok(previews)
}

fn entry_kind<'a>(snapshot: &'a SnapshotManifest, path: &str) -> Option<&'a str> {
    if snapshot.files.contains_key(path) {
        Some("file")
    } else if snapshot.symlinks.contains_key(path) {
        Some("symlink")
    } else if snapshot.directories.contains(path) {
        Some("directory")
    } else {
        None
    }
}

#[cfg(unix)]
fn create_symlink(target: &str, destination: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, destination).with_context(|| {
        format!(
            "creating replay symlink {} -> {}",
            destination.display(),
            target
        )
    })
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, destination: &Path) -> Result<()> {
    bail!(
        "replay materialization of symlinks is unsupported on this platform: {}",
        destination.display()
    )
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    let mut mode = permissions.mode();
    if executable {
        mode |= 0o111;
    } else {
        mode &= !0o111;
    }
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

fn simple_text_diff(original: &str, replay: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in original.lines() {
        if !replay.lines().any(|candidate| candidate == line) {
            lines.push(format!("-{line}"));
        }
    }
    for line in replay.lines() {
        if !original.lines().any(|candidate| candidate == line) {
            lines.push(format!("+{line}"));
        }
    }
    lines
}
