use anyhow::{anyhow, Context, Result};
use rewind_core::object_store::sha256_hex;
use rewind_core::path_safety::validate_relative_path;
use rewind_core::snapshot::{create_snapshot, write_snapshot};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

static CARGO_RUN_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

struct Lab {
    dir: TempDir,
    manifest: PathBuf,
}

impl Lab {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
            manifest: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn rewind(&self, args: &[&str]) -> Result<Output> {
        let output = self.rewind_raw(args)?;

        if !output.status.success() {
            return Err(anyhow!(
                "rewind {:?} failed\nstdout:\n{}\nstderr:\n{}",
                args,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(output)
    }

    fn rewind_raw(&self, args: &[&str]) -> Result<Output> {
        let _guard = CARGO_RUN_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("cargo run lock poisoned");
        Command::new(env!("CARGO"))
            .arg("run")
            .arg("--quiet")
            .arg("--manifest-path")
            .arg(&self.manifest)
            .arg("-p")
            .arg("rewind-cli")
            .arg("--")
            .args(args)
            .current_dir(self.path())
            .output()
            .context("running rewind through cargo")
    }

    fn init(&self) -> Result<()> {
        self.rewind(&["init"])?;
        Ok(())
    }

    fn run(&self, command: &[&str]) -> Result<()> {
        let mut args = vec!["run", "--"];
        args.extend(command);
        self.rewind(&args)?;
        Ok(())
    }

    fn run_allow_dirty(&self, command: &[&str]) -> Result<()> {
        let mut args = vec!["run", "--allow-dirty", "--"];
        args.extend(command);
        self.rewind(&args)?;
        Ok(())
    }

    fn event_count(&self) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
    }

    fn first_event_counts(&self) -> Result<(i64, i64, i64)> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT created_count, modified_count, deleted_count FROM events ORDER BY id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?)
    }

    fn latest_event_exit_code(&self) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT exit_code FROM events ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?)
    }

    fn undone_count(&self) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(
            conn.query_row("SELECT COUNT(*) FROM events WHERE undone = 1", [], |row| {
                row.get(0)
            })?,
        )
    }

    fn head_snapshot(&self) -> Result<String> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT value FROM workspace_state WHERE key = 'head_snapshot'",
            [],
            |row| row.get(0),
        )?)
    }

    fn latest_event_snapshots(&self) -> Result<(String, String)> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT before_snapshot, after_snapshot FROM events ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    fn latest_event_kind_and_command(&self) -> Result<(String, String)> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT kind, command FROM events ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    fn latest_event_started_dirty(&self) -> Result<bool> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        let started_dirty: i64 = conn.query_row(
            "SELECT started_dirty FROM events ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )?;
        Ok(started_dirty != 0)
    }

    fn file_change_count(&self, event_id: i64, change_type: &str) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM file_changes WHERE event_id = ?1 AND change_type = ?2",
            (event_id, change_type),
            |row| row.get(0),
        )?)
    }

    fn snapshot_manifest_count(&self) -> Result<usize> {
        Ok(fs::read_dir(self.path().join(".rewind/snapshots"))?.count())
    }

    fn object_count(&self) -> Result<usize> {
        Ok(fs::read_dir(self.path().join(".rewind/objects"))?.count())
    }

    fn checkpoint_snapshot(&self, name: &str) -> Result<String> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT snapshot_id FROM checkpoints WHERE name = ?1",
            [name],
            |row| row.get(0),
        )?)
    }

    fn checkpoint_count(&self) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row("SELECT COUNT(*) FROM checkpoints", [], |row| row.get(0))?)
    }

    fn event_kind(&self, event_id: i64) -> Result<String> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(
            conn.query_row("SELECT kind FROM events WHERE id = ?1", [event_id], |row| {
                row.get(0)
            })?,
        )
    }

    fn snapshot_path(&self, snapshot_id: &str) -> PathBuf {
        self.path()
            .join(".rewind/snapshots")
            .join(format!("{snapshot_id}.json"))
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.path().join(".rewind/objects").join(hash)
    }

    fn active_journal_path(&self) -> PathBuf {
        self.path().join(".rewind/journal/active.json")
    }

    fn completed_journal_count(&self) -> Result<usize> {
        let path = self.path().join(".rewind/journal/completed");
        if !path.exists() {
            return Ok(0);
        }
        Ok(fs::read_dir(path)?.count())
    }

    fn load_snapshot_json(&self, snapshot_id: &str) -> Result<Value> {
        Ok(serde_json::from_str(&fs::read_to_string(
            self.snapshot_path(snapshot_id),
        )?)?)
    }

    fn overwrite_snapshot_json(&self, snapshot_id: &str, value: &Value) -> Result<()> {
        fs::write(
            self.snapshot_path(snapshot_id),
            serde_json::to_vec_pretty(value)?,
        )?;
        Ok(())
    }

    fn first_file_hash_and_size(&self, snapshot_id: &str) -> Result<(String, u64)> {
        let manifest = self.load_snapshot_json(snapshot_id)?;
        let files = manifest["files"].as_object().context("files object")?;
        let (_, entry) = files.iter().next().context("file entry")?;
        Ok((
            entry["hash"].as_str().context("hash")?.to_owned(),
            entry["size"].as_u64().context("size")?,
        ))
    }

    fn create_unreferenced_object(&self, bytes: &[u8]) -> Result<String> {
        let hash = sha256_hex(bytes);
        fs::write(self.object_path(&hash), bytes)?;
        Ok(hash)
    }

    fn create_unreferenced_snapshot(&self) -> Result<String> {
        fs::write(self.path().join("orphan.txt"), "orphan\n")?;
        let snapshot = create_snapshot(self.path())?;
        write_snapshot(self.path(), &snapshot)?;
        fs::remove_file(self.path().join("orphan.txt"))?;
        Ok(snapshot.id)
    }
}

#[test]
fn init_creates_expected_structure() -> Result<()> {
    let lab = Lab::new();

    lab.init()?;

    assert!(lab.path().join(".rewind").is_dir());
    assert!(lab.path().join(".rewind/objects").is_dir());
    assert!(lab.path().join(".rewind/snapshots").is_dir());
    assert!(lab.path().join(".rewind/events.db").is_file());
    Ok(())
}

#[test]
fn creating_a_file_through_run_is_recorded() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    assert_eq!(lab.first_event_counts()?, (1, 0, 0));
    Ok(())
}

#[test]
fn modifying_a_file_through_run_is_recorded() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;

    lab.run(&["sh", "-c", "echo goodbye > notes.txt"])?;

    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        "goodbye\n"
    );
    assert_eq!(lab.first_event_counts()?, (0, 1, 0));
    Ok(())
}

#[test]
fn deleting_a_file_through_run_is_recorded() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;

    lab.run(&["rm", "notes.txt"])?;

    assert!(!lab.path().join("notes.txt").exists());
    assert_eq!(lab.first_event_counts()?, (0, 0, 1));
    Ok(())
}

#[test]
fn undo_reverses_a_file_creation() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    lab.rewind(&["undo"])?;

    assert!(!lab.path().join("notes.txt").exists());
    Ok(())
}

#[test]
fn undo_reverses_a_file_modification() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;
    lab.run(&["sh", "-c", "echo goodbye > notes.txt"])?;

    lab.rewind(&["undo"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    Ok(())
}

#[test]
fn undo_reverses_a_file_deletion() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;
    lab.run(&["rm", "notes.txt"])?;

    lab.rewind(&["undo"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    Ok(())
}

#[test]
fn rewind_directory_is_never_included_in_snapshots() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    for entry in fs::read_dir(lab.path().join(".rewind/snapshots"))? {
        let manifest: Value = serde_json::from_str(&fs::read_to_string(entry?.path())?)?;
        let files = manifest["files"]
            .as_object()
            .context("manifest files object")?;
        assert!(files.keys().all(|path| !path.starts_with(".rewind/")));
        assert!(files.keys().all(|path| path != ".rewind"));
    }

    Ok(())
}

#[test]
fn failed_command_still_records_an_event_with_nonzero_exit_code() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind_raw(&[
        "run",
        "--",
        "sh",
        "-c",
        "echo before-fail > failed.txt; exit 7",
    ])?;

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(lab.event_count()?, 1);
    assert_eq!(lab.latest_event_exit_code()?, 7);
    assert_eq!(
        fs::read_to_string(lab.path().join("failed.txt"))?,
        "before-fail\n"
    );
    Ok(())
}

#[test]
fn status_reports_clean_immediately_after_init() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind(&["status"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Rewind worktree clean."));
    assert!(stdout.contains("Head snapshot:"));
    Ok(())
}

#[test]
fn status_reports_added_files_after_manual_create() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind(&["status"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Rewind worktree dirty."));
    assert!(stdout.contains("Added:"));
    assert!(stdout.contains("  scratch.txt"));
    Ok(())
}

#[test]
fn status_reports_modified_files_after_manual_edit() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "manual\n")?;

    let output = lab.rewind(&["status"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Modified:"));
    assert!(stdout.contains("  notes.txt"));
    Ok(())
}

#[test]
fn status_reports_deleted_files_after_manual_delete() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.init()?;
    fs::remove_file(lab.path().join("notes.txt"))?;

    let output = lab.rewind(&["status"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Deleted:"));
    assert!(stdout.contains("  notes.txt"));
    Ok(())
}

#[test]
fn undo_refuses_if_untracked_file_added_after_latest_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["undo"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Rewind worktree dirty."));
    assert!(stdout.contains("scratch.txt"));
    assert!(lab.path().join("notes.txt").exists());
    assert_eq!(lab.undone_count()?, 0);
    Ok(())
}

#[test]
fn undo_refuses_if_tracked_file_modified_after_latest_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    fs::write(lab.path().join("notes.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["undo"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Modified:"));
    assert!(stdout.contains("notes.txt"));
    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        "manual\n"
    );
    assert_eq!(lab.undone_count()?, 0);
    Ok(())
}

#[test]
fn undo_refuses_if_tracked_file_deleted_after_latest_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    fs::remove_file(lab.path().join("notes.txt"))?;

    let output = lab.rewind_raw(&["undo"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Deleted:"));
    assert!(stdout.contains("notes.txt"));
    assert!(!lab.path().join("notes.txt").exists());
    assert_eq!(lab.undone_count()?, 0);
    Ok(())
}

#[test]
fn undo_dry_run_does_not_modify_files_or_mark_event_undone() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let output = lab.rewind(&["undo", "--dry-run"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Restore plan:"));
    assert!(stdout.contains("Remove files:"));
    assert!(stdout.contains("notes.txt"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    assert_eq!(lab.undone_count()?, 0);
    Ok(())
}

#[test]
fn empty_directories_are_captured_in_snapshots() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir empty-dir"])?;

    let (_, after_snapshot) = lab.latest_event_snapshots()?;
    let manifest_path = lab
        .path()
        .join(".rewind/snapshots")
        .join(format!("{after_snapshot}.json"));
    let manifest: Value = serde_json::from_str(&fs::read_to_string(manifest_path)?)?;
    let directories = manifest["directories"]
        .as_array()
        .context("manifest directories array")?;

    assert!(directories.iter().any(|entry| entry == "empty-dir"));
    Ok(())
}

#[test]
fn empty_directories_are_restored_after_undo() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir empty-dir"])?;
    lab.run(&["rmdir", "empty-dir"])?;

    lab.rewind(&["undo"])?;

    assert!(lab.path().join("empty-dir").is_dir());
    Ok(())
}

#[test]
fn nested_directories_and_nested_files_restore_correctly() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p src/nested && echo hello > src/nested/notes.txt",
    ])?;

    lab.rewind(&["undo"])?;

    assert!(!lab.path().join("src/nested/notes.txt").exists());
    assert!(!lab.path().join("src/nested").exists());
    assert!(!lab.path().join("src").exists());
    Ok(())
}

#[test]
fn rewind_directory_is_never_included_in_status_output() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;

    let output = lab.rewind(&["status"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Rewind worktree clean."));
    assert!(!stdout.contains("internal.txt"));
    Ok(())
}

#[test]
fn undo_updates_head_snapshot_so_repeated_undo_works() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo one > notes.txt"])?;
    lab.run(&["sh", "-c", "echo two > notes.txt"])?;

    lab.rewind(&["undo"])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "one\n");
    let head_after_first_undo = lab.head_snapshot()?;
    let (second_before, _) = lab.latest_event_snapshots()?;
    assert_eq!(head_after_first_undo, second_before);

    lab.rewind(&["undo"])?;

    assert!(!lab.path().join("notes.txt").exists());
    assert_eq!(lab.undone_count()?, 2);
    Ok(())
}

#[test]
fn path_validation_rejects_unsafe_snapshot_paths() {
    assert!(validate_relative_path("/absolute/path").is_err());
    assert!(validate_relative_path("../outside.txt").is_err());
    assert!(validate_relative_path("nested/../../outside.txt").is_err());
    assert!(validate_relative_path(".rewind/events.db").is_err());
    assert!(validate_relative_path(".rewind/objects/foo").is_err());
}

#[test]
fn timeline_shows_event_ids_commands_snapshot_transitions_and_head() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    let output = lab.rewind(&["timeline"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("ID"));
    assert!(stdout.contains("KIND"));
    assert!(stdout.contains("SNAPSHOT TRANSITION"));
    assert!(stdout.contains("HEAD:"));
    assert!(stdout.contains("1   run"));
    assert!(stdout.contains("2   run"));
    assert!(stdout.contains("echo good > notes.txt"));
    assert!(stdout.contains("echo bad > notes.txt"));
    assert!(stdout.contains(" -> "));
    Ok(())
}

#[test]
fn diff_shows_created_files() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let output = lab.rewind(&["diff", "1"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Event 1"));
    assert!(stdout.contains("Created:"));
    assert!(stdout.contains("notes.txt"));
    Ok(())
}

#[test]
fn diff_shows_modified_files() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    let output = lab.rewind(&["diff", "2"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Modified:"));
    assert!(stdout.contains("notes.txt"));
    Ok(())
}

#[test]
fn diff_shows_deleted_files() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.run(&["rm", "notes.txt"])?;

    let output = lab.rewind(&["diff", "2"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Deleted:"));
    assert!(stdout.contains("notes.txt"));
    Ok(())
}

#[test]
fn diff_shows_text_diff_for_small_modified_text_file() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    let output = lab.rewind(&["diff", "2"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("--- notes.txt before event 2"));
    assert!(stdout.contains("+++ notes.txt after event 2"));
    assert!(stdout.contains("-good"));
    assert!(stdout.contains("+bad"));
    Ok(())
}

#[test]
fn diff_skips_textual_diff_for_invalid_utf8_content() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "printf '\\377\\376' > data.bin"])?;
    lab.run(&["sh", "-c", "printf '\\377\\375' > data.bin"])?;

    let output = lab.rewind(&["diff", "2"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Binary or non-UTF8 content changed; textual diff skipped."));
    Ok(())
}

#[test]
fn restore_file_before_event_restores_previous_version() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    lab.rewind(&["restore", "notes.txt", "--before", "2"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "good\n");
    Ok(())
}

#[test]
fn restore_file_after_event_restores_later_version() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    lab.rewind(&["restore", "notes.txt", "--after", "1"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "good\n");
    Ok(())
}

#[test]
fn restore_file_before_event_creates_a_new_restore_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    lab.rewind(&["restore", "notes.txt", "--before", "2"])?;
    let (kind, command) = lab.latest_event_kind_and_command()?;

    assert_eq!(lab.event_count()?, 3);
    assert_eq!(kind, "restore");
    assert!(command.contains("restore notes.txt --before 2"));
    assert_eq!(lab.undone_count()?, 0);
    Ok(())
}

#[test]
fn targeted_restore_event_can_be_undone_with_normal_undo() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    lab.rewind(&["restore", "notes.txt", "--before", "2"])?;

    lab.rewind(&["undo"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "bad\n");
    assert_eq!(lab.undone_count()?, 1);
    Ok(())
}

#[test]
fn targeted_restore_refuses_to_run_when_worktree_is_dirty() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["restore", "notes.txt", "--before", "2"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Rewind worktree dirty."));
    assert!(stdout.contains("scratch.txt"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "bad\n");
    assert_eq!(lab.event_count()?, 2);
    Ok(())
}

#[test]
fn targeted_restore_dry_run_does_not_modify_files_create_event_or_update_head() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    let head_before = lab.head_snapshot()?;

    let output = lab.rewind(&["restore", "notes.txt", "--before", "2", "--dry-run"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Restore plan for notes.txt from before event 2:"));
    assert!(stdout.contains("Would write:"));
    assert!(stdout.contains("notes.txt"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "bad\n");
    assert_eq!(lab.event_count()?, 2);
    assert_eq!(lab.head_snapshot()?, head_before);
    Ok(())
}

#[test]
fn restoring_a_deleted_file_works() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.run(&["rm", "notes.txt"])?;

    lab.rewind(&["restore", "notes.txt", "--before", "2"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    Ok(())
}

#[test]
fn restoring_file_to_state_where_it_did_not_exist_removes_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    lab.rewind(&["restore", "notes.txt", "--before", "1"])?;

    assert!(!lab.path().join("notes.txt").exists());
    Ok(())
}

#[test]
fn restoring_directory_subtree_works() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p src/nested && echo one > src/nested/notes.txt",
    ])?;
    lab.run(&[
        "sh",
        "-c",
        "echo two > src/nested/notes.txt && echo extra > src/extra.txt",
    ])?;

    lab.rewind(&["restore", "src", "--before", "2"])?;

    assert_eq!(
        fs::read_to_string(lab.path().join("src/nested/notes.txt"))?,
        "one\n"
    );
    assert!(!lab.path().join("src/extra.txt").exists());
    Ok(())
}

#[test]
fn restoring_empty_directory_works() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir empty-dir"])?;
    lab.run(&["rmdir", "empty-dir"])?;

    lab.rewind(&["restore", "empty-dir", "--before", "2"])?;

    assert!(lab.path().join("empty-dir").is_dir());
    Ok(())
}

#[test]
fn restoring_directory_to_state_where_it_did_not_exist_removes_it_if_empty() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir empty-dir"])?;

    lab.rewind(&["restore", "empty-dir", "--before", "1"])?;

    assert!(!lab.path().join("empty-dir").exists());
    Ok(())
}

#[test]
fn invalid_restore_paths_are_rejected() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let absolute = lab.path().join("notes.txt").to_string_lossy().to_string();
    for path in [absolute.as_str(), "../outside.txt", ".rewind/events.db"] {
        let output = lab.rewind_raw(&["restore", path, "--before", "1"])?;
        assert!(!output.status.success(), "path should be rejected: {path}");
    }
    Ok(())
}

#[test]
fn rewind_directory_never_appears_in_timeline_diff_restore_plan_or_status() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let timeline = String::from_utf8(lab.rewind(&["timeline"])?.stdout)?;
    let diff = String::from_utf8(lab.rewind(&["diff", "1"])?.stdout)?;
    let plan = String::from_utf8(
        lab.rewind(&["restore", "notes.txt", "--before", "1", "--dry-run"])?
            .stdout,
    )?;
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;

    assert!(!timeline.contains(".rewind"));
    assert!(!diff.contains(".rewind"));
    assert!(!plan.contains(".rewind"));
    assert!(!status.contains(".rewind"));
    Ok(())
}

#[test]
fn run_refuses_when_untracked_file_was_manually_added() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["run", "--", "sh", "-c", "echo later > other.txt"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Cannot run command: Rewind worktree is dirty."));
    assert!(stdout.contains("scratch.txt"));
    assert!(!lab.path().join("other.txt").exists());
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn run_refuses_when_tracked_file_was_manually_modified() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "original\n")?;
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["run", "--", "sh", "-c", "echo later > other.txt"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Modified:"));
    assert!(stdout.contains("notes.txt"));
    assert!(!lab.path().join("other.txt").exists());
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn run_refuses_when_tracked_file_was_manually_deleted() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "original\n")?;
    lab.init()?;
    fs::remove_file(lab.path().join("notes.txt"))?;

    let output = lab.rewind_raw(&["run", "--", "sh", "-c", "echo later > other.txt"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Deleted:"));
    assert!(stdout.contains("notes.txt"));
    assert!(!lab.path().join("other.txt").exists());
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn run_dirty_refusal_suggests_commit_command() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["run", "--", "sh", "-c", "echo later > other.txt"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("rewind commit -m \"describe your changes\""));
    assert!(stdout.contains("Or discard/restore them manually, then try again."));
    Ok(())
}

#[test]
fn run_allow_dirty_allows_running_from_dirty_worktree_and_marks_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    lab.run_allow_dirty(&["sh", "-c", "echo later > other.txt"])?;

    assert_eq!(fs::read_to_string(lab.path().join("other.txt"))?, "later\n");
    assert_eq!(lab.event_count()?, 1);
    assert!(lab.latest_event_started_dirty()?);
    Ok(())
}

#[test]
fn show_displays_dirty_start_information() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;
    lab.run_allow_dirty(&["sh", "-c", "echo later > other.txt"])?;

    let output = lab.rewind(&["show", "1"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Started from dirty worktree: yes"));
    Ok(())
}

#[test]
fn commit_records_added_files_and_updates_head() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let head_before = lab.head_snapshot()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    lab.rewind(&["commit", "-m", "add scratch"])?;

    assert_ne!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, 1);
    assert_eq!(lab.file_change_count(1, "created")?, 1);
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree clean."));
    Ok(())
}

#[test]
fn commit_records_modified_files() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "original\n")?;
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "manual\n")?;

    lab.rewind(&["commit", "-m", "manual edit"])?;

    assert_eq!(lab.file_change_count(1, "modified")?, 1);
    Ok(())
}

#[test]
fn commit_records_deleted_files() -> Result<()> {
    let lab = Lab::new();
    fs::write(lab.path().join("notes.txt"), "original\n")?;
    lab.init()?;
    fs::remove_file(lab.path().join("notes.txt"))?;

    lab.rewind(&["commit", "-m", "manual delete"])?;

    assert_eq!(lab.file_change_count(1, "deleted")?, 1);
    Ok(())
}

#[test]
fn commit_creates_commit_event_kind_and_file_changes() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    lab.rewind(&["commit", "-m", "manual capture"])?;
    let (kind, command) = lab.latest_event_kind_and_command()?;

    assert_eq!(kind, "commit");
    assert!(command.contains("commit: manual capture"));
    assert_eq!(lab.file_change_count(1, "created")?, 1);
    assert!(!lab.latest_event_started_dirty()?);
    Ok(())
}

#[test]
fn commit_dry_run_does_not_create_event_update_head_or_write_storage() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind(&["commit", "--dry-run", "-m", "manual capture"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Would commit manual changes: manual capture"));
    assert!(stdout.contains("Added:"));
    assert!(stdout.contains("scratch.txt"));
    assert_eq!(lab.event_count()?, 0);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    Ok(())
}

#[test]
fn commit_on_clean_worktree_creates_no_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind(&["commit", "-m", "nothing"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Nothing to commit. Rewind worktree clean."));
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn undo_can_undo_commit_event_and_restore_head_snapshot() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo original > notes.txt"])?;
    let head_before_commit = lab.head_snapshot()?;
    fs::write(lab.path().join("notes.txt"), "manual\n")?;
    lab.rewind(&["commit", "-m", "manual edit"])?;

    lab.rewind(&["undo"])?;

    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        "original\n"
    );
    assert_eq!(lab.head_snapshot()?, head_before_commit);
    assert_eq!(lab.undone_count()?, 1);
    Ok(())
}

#[test]
fn rewind_directory_changes_are_ignored_by_status_and_commit() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;

    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    let commit = String::from_utf8(lab.rewind(&["commit", "-m", "ignored"])?.stdout)?;

    assert!(status.contains("Rewind worktree clean."));
    assert!(commit.contains("Nothing to commit. Rewind worktree clean."));
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn history_and_timeline_display_dirty_start_information() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;
    lab.run_allow_dirty(&["sh", "-c", "echo later > other.txt"])?;

    let history = String::from_utf8(lab.rewind(&["history"])?.stdout)?;
    let timeline = String::from_utf8(lab.rewind(&["timeline"])?.stdout)?;

    assert!(history.contains("DIRTY"));
    assert!(history.contains("yes"));
    assert!(timeline.contains("DIRTY"));
    assert!(timeline.contains("yes"));
    Ok(())
}

#[test]
fn checkpoint_create_points_to_current_head_and_list_show_display_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    let head = lab.head_snapshot()?;

    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    let list = String::from_utf8(lab.rewind(&["checkpoint", "list"])?.stdout)?;
    let show = String::from_utf8(lab.rewind(&["checkpoint", "show", "v1"])?.stdout)?;

    assert_eq!(lab.checkpoint_snapshot("v1")?, head);
    assert!(list.contains("v1"));
    assert!(list.contains(&head.chars().take(6).collect::<String>()));
    assert!(list.contains("Version one"));
    assert!(show.contains("Name: v1"));
    assert!(show.contains("Points to HEAD: yes"));
    assert_eq!(lab.event_count()?, 1);
    Ok(())
}

#[test]
fn checkpoint_delete_removes_metadata_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.rewind(&["checkpoint", "create", "safe", "-m", "Safe point"])?;

    lab.rewind(&["checkpoint", "delete", "safe"])?;

    assert_eq!(lab.checkpoint_count()?, 0);
    assert_eq!(lab.event_count()?, 0);
    Ok(())
}

#[test]
fn checkpoint_create_refuses_dirty_worktree_and_suggests_commit() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["checkpoint", "create", "dirty", "-m", "Dirty"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Rewind worktree dirty."));
    assert!(stdout.contains("scratch.txt"));
    assert!(stdout.contains("rewind commit -m"));
    assert_eq!(lab.checkpoint_count()?, 0);
    Ok(())
}

#[test]
fn checkpoint_duplicate_fails_unless_force_updates_snapshot() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "stable", "-m", "v1"])?;
    let first = lab.checkpoint_snapshot("stable")?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    let output = lab.rewind_raw(&["checkpoint", "create", "stable", "-m", "duplicate"])?;
    assert!(!output.status.success());
    lab.rewind(&["checkpoint", "create", "--force", "stable", "-m", "v2"])?;

    assert_ne!(lab.checkpoint_snapshot("stable")?, first);
    assert_eq!(lab.checkpoint_snapshot("stable")?, lab.head_snapshot()?);
    Ok(())
}

#[test]
fn invalid_checkpoint_names_are_rejected() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let long = "a".repeat(81);
    for name in [
        "",
        "bad/name",
        "bad\\name",
        "bad name",
        "..",
        "nested..name",
        long.as_str(),
    ] {
        let output = lab.rewind_raw(&["checkpoint", "create", name, "-m", "bad"])?;
        assert!(
            !output.status.success(),
            "checkpoint name should fail: {name}"
        );
    }
    assert_eq!(lab.checkpoint_count()?, 0);
    Ok(())
}

#[test]
fn checkout_checkpoint_restores_worktree_and_creates_undoable_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    lab.rewind(&["checkout", "--checkpoint", "v1"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v1\n");
    assert_eq!(lab.event_kind(3)?, "checkout");
    assert_eq!(lab.head_snapshot()?, lab.checkpoint_snapshot("v1")?);
    assert_eq!(lab.undone_count()?, 0);

    lab.rewind(&["undo"])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v2\n");
    Ok(())
}

#[test]
fn checkout_before_after_and_snapshot_targets_restore_expected_versions() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    let (_, event1_after) = lab.latest_event_snapshots()?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    lab.rewind(&["checkout", "--before", "2"])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v1\n");

    lab.rewind(&["checkout", "--after", "2"])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v2\n");

    lab.rewind(&["checkout", "--snapshot", &event1_after])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v1\n");
    Ok(())
}

#[test]
fn checkout_refuses_dirty_worktrees() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    fs::write(lab.path().join("scratch.txt"), "manual\n")?;

    let output = lab.rewind_raw(&["checkout", "--checkpoint", "v1"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(!output.status.success());
    assert!(stdout.contains("Rewind worktree dirty."));
    assert!(stdout.contains("scratch.txt"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v2\n");
    assert_eq!(lab.event_count()?, 2);
    Ok(())
}

#[test]
fn checkout_dry_run_does_not_modify_files_event_or_head() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    let head_before = lab.head_snapshot()?;

    let output = lab.rewind(&["checkout", "--checkpoint", "v1", "--dry-run"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Checkout plan"));
    assert!(stdout.contains("Would write:"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v2\n");
    assert_eq!(lab.event_count()?, 2);
    assert_eq!(lab.head_snapshot()?, head_before);
    Ok(())
}

#[test]
fn checkout_to_current_head_is_noop() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;

    let output = lab.rewind(&["checkout", "--checkpoint", "v1"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("already at"));
    assert_eq!(lab.event_count()?, 1);
    Ok(())
}

#[test]
fn checkout_restores_deleted_files_and_removes_extra_files() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo keep > keep.txt"])?;
    lab.rewind(&["checkpoint", "create", "base", "-m", "Base"])?;
    lab.run(&["sh", "-c", "rm keep.txt && echo extra > extra.txt"])?;

    lab.rewind(&["checkout", "--checkpoint", "base"])?;

    assert_eq!(fs::read_to_string(lab.path().join("keep.txt"))?, "keep\n");
    assert!(!lab.path().join("extra.txt").exists());
    Ok(())
}

#[test]
fn checkout_restores_and_removes_empty_directories() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir empty-dir"])?;
    lab.rewind(&["checkpoint", "create", "with-empty", "-m", "With empty"])?;
    lab.run(&["sh", "-c", "rmdir empty-dir && mkdir extra-empty"])?;

    lab.rewind(&["checkout", "--checkpoint", "with-empty"])?;

    assert!(lab.path().join("empty-dir").is_dir());
    assert!(!lab.path().join("extra-empty").exists());
    Ok(())
}

#[test]
fn checkout_handles_nested_directories_and_files() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p src/nested && echo v1 > src/nested/notes.txt",
    ])?;
    lab.rewind(&["checkpoint", "create", "nested-v1", "-m", "Nested v1"])?;
    lab.run(&[
        "sh",
        "-c",
        "echo v2 > src/nested/notes.txt && echo extra > src/extra.txt",
    ])?;

    lab.rewind(&["checkout", "--checkpoint", "nested-v1"])?;

    assert_eq!(
        fs::read_to_string(lab.path().join("src/nested/notes.txt"))?,
        "v1\n"
    );
    assert!(!lab.path().join("src/extra.txt").exists());
    Ok(())
}

#[test]
fn checkout_argument_validation_rejects_zero_or_multiple_targets() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;

    let none = lab.rewind_raw(&["checkout"])?;
    let many = lab.rewind_raw(&["checkout", "--checkpoint", "v1", "--before", "1"])?;

    assert!(!none.status.success());
    assert!(!many.status.success());
    Ok(())
}

#[test]
fn timeline_history_show_display_checkpoint_and_checkout_information() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    lab.rewind(&["checkout", "--checkpoint", "v1"])?;

    let timeline = String::from_utf8(lab.rewind(&["timeline"])?.stdout)?;
    let history = String::from_utf8(lab.rewind(&["history"])?.stdout)?;
    let show = String::from_utf8(lab.rewind(&["show", "3"])?.stdout)?;

    assert!(timeline.contains("Checkpoints:"));
    assert!(timeline.contains("v1"));
    assert!(timeline.contains("->"));
    assert!(timeline.contains("checkout"));
    assert!(history.contains("checkout"));
    assert!(show.contains("Kind: checkout"));
    Ok(())
}

#[test]
fn rewind_directory_never_appears_in_checkpoint_checkout_plan_timeline_diff_or_status() -> Result<()>
{
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&["checkpoint", "create", "v1", "-m", "Version one"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    let list = String::from_utf8(lab.rewind(&["checkpoint", "list"])?.stdout)?;
    let plan = String::from_utf8(
        lab.rewind(&["checkout", "--checkpoint", "v1", "--dry-run"])?
            .stdout,
    )?;
    let timeline = String::from_utf8(lab.rewind(&["timeline"])?.stdout)?;
    let diff = String::from_utf8(lab.rewind(&["diff", "1"])?.stdout)?;
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;

    assert!(!list.contains(".rewind"));
    assert!(!plan.contains(".rewind"));
    assert!(!timeline.contains(".rewind"));
    assert!(!diff.contains(".rewind"));
    assert!(!status.contains(".rewind"));
    Ok(())
}

#[test]
fn verify_succeeds_on_clean_initialized_workspace() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind(&["verify"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(stdout.contains("Rewind verify: OK"));
    assert!(stdout.contains("Errors:"));
    assert!(stdout.contains("Warnings:"));
    Ok(())
}

#[test]
fn verify_succeeds_after_history_checkpoint_checkout_and_undo_flows() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    fs::write(lab.path().join("manual.txt"), "manual\n")?;
    lab.rewind(&["commit", "-m", "manual"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    lab.rewind(&["restore", "notes.txt", "--before", "3"])?;
    lab.rewind(&["checkpoint", "create", "safe", "-m", "Safe"])?;
    lab.run(&["sh", "-c", "echo v3 > notes.txt"])?;
    lab.rewind(&["checkout", "--checkpoint", "safe"])?;
    lab.rewind(&["undo"])?;

    let stdout = String::from_utf8(lab.rewind(&["verify"])?.stdout)?;

    assert!(stdout.contains("Rewind verify: OK"));
    assert!(stdout.contains("Checkpoints:"));
    Ok(())
}

#[test]
fn verify_detects_missing_and_corrupted_referenced_objects() -> Result<()> {
    let missing = Lab::new();
    missing.init()?;
    missing.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let (_, after) = missing.latest_event_snapshots()?;
    let (hash, _) = missing.first_file_hash_and_size(&after)?;
    fs::remove_file(missing.object_path(&hash))?;

    let output = missing.rewind_raw(&["verify"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("Missing object"));
    assert!(stdout.contains(&hash));

    let corrupted = Lab::new();
    corrupted.init()?;
    corrupted.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let (_, after) = corrupted.latest_event_snapshots()?;
    let (hash, _) = corrupted.first_file_hash_and_size(&after)?;
    fs::write(corrupted.object_path(&hash), "corrupt\n")?;

    let output = corrupted.rewind_raw(&["verify"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("hash mismatch"));
    assert!(stdout.contains(&hash));
    Ok(())
}

#[test]
fn verify_detects_missing_referenced_snapshots_and_bad_checkpoints() -> Result<()> {
    let missing_snapshot = Lab::new();
    missing_snapshot.init()?;
    missing_snapshot.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let head = missing_snapshot.head_snapshot()?;
    fs::remove_file(missing_snapshot.snapshot_path(&head))?;

    let output = missing_snapshot.rewind_raw(&["verify"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("missing snapshot"));
    assert!(stdout.contains(&head));

    let bad_checkpoint = Lab::new();
    bad_checkpoint.init()?;
    bad_checkpoint.rewind(&["checkpoint", "create", "safe", "-m", "Safe"])?;
    let conn = Connection::open(bad_checkpoint.path().join(".rewind/events.db"))?;
    conn.execute(
        "UPDATE checkpoints SET snapshot_id = 'missing-snapshot' WHERE name = 'safe'",
        [],
    )?;

    let output = bad_checkpoint.rewind_raw(&["verify"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!output.status.success());
    assert!(stdout.contains("Checkpoint safe points to missing snapshot missing-snapshot"));
    Ok(())
}

#[test]
fn verify_detects_invalid_paths_inside_referenced_snapshot_manifest() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let head = lab.head_snapshot()?;
    let mut manifest = lab.load_snapshot_json(&head)?;
    let (hash, size) = lab.first_file_hash_and_size(&head)?;

    manifest["files"]["/absolute/path"] = json!({ "hash": hash, "size": size });
    manifest["files"]["nested/../../outside.txt"] = json!({ "hash": hash, "size": size });
    manifest["files"][".rewind/events.db"] = json!({ "hash": hash, "size": size });
    lab.overwrite_snapshot_json(&head, &manifest)?;

    let output = lab.rewind_raw(&["verify"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(!output.status.success());
    assert!(stdout.contains("/absolute/path"));
    assert!(stdout.contains("nested/../../outside.txt"));
    assert!(stdout.contains(".rewind/events.db"));
    Ok(())
}

#[test]
fn verify_reports_unreferenced_storage_as_warnings_and_strict_fails() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let object_hash = lab.create_unreferenced_object(b"loose object\n")?;
    let snapshot_id = lab.create_unreferenced_snapshot()?;

    let output = lab.rewind(&["verify"])?;
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Rewind verify: OK"));
    assert!(stdout.contains("Unreferenced object"));
    assert!(stdout.contains(&object_hash));
    assert!(stdout.contains("Unreferenced snapshot"));
    assert!(stdout.contains(&snapshot_id));

    let strict = lab.rewind_raw(&["verify", "--strict"])?;
    assert!(!strict.status.success());
    assert!(String::from_utf8_lossy(&strict.stdout).contains("Warnings:"));
    Ok(())
}

#[test]
fn stats_prints_history_storage_counts_and_reclaimable_bytes() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    fs::write(lab.path().join("manual.txt"), "manual\n")?;
    lab.rewind(&["commit", "-m", "manual"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    lab.rewind(&["restore", "notes.txt", "--before", "3"])?;
    lab.rewind(&["checkpoint", "create", "safe", "-m", "Safe"])?;
    lab.run(&["sh", "-c", "echo v3 > notes.txt"])?;
    lab.rewind(&["checkout", "--checkpoint", "safe"])?;
    lab.create_unreferenced_object(b"loose object\n")?;
    lab.create_unreferenced_snapshot()?;

    let stdout = String::from_utf8(lab.rewind(&["stats"])?.stdout)?;

    assert!(stdout.contains("Events:"));
    assert!(stdout.contains("run:"));
    assert!(stdout.contains("commit:"));
    assert!(stdout.contains("restore:"));
    assert!(stdout.contains("checkout:"));
    assert!(stdout.contains("Snapshots:"));
    assert!(stdout.contains("reachable:"));
    assert!(stdout.contains("unreferenced:"));
    assert!(stdout.contains("Objects:"));
    assert!(stdout.contains("reclaimable bytes:"));
    assert!(stdout.contains("Checkpoints:"));
    Ok(())
}

#[test]
fn gc_dry_run_does_not_delete_unreferenced_storage_and_yes_deletes_only_unreferenced() -> Result<()>
{
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo keep > notes.txt"])?;
    let (_, reachable_snapshot) = lab.latest_event_snapshots()?;
    let (reachable_object, _) = lab.first_file_hash_and_size(&reachable_snapshot)?;
    let loose_object = lab.create_unreferenced_object(b"loose object\n")?;
    let loose_snapshot = lab.create_unreferenced_snapshot()?;

    let dry_run = String::from_utf8(lab.rewind(&["gc"])?.stdout)?;
    assert!(dry_run.contains("dry run"));
    assert!(lab.object_path(&loose_object).exists());
    assert!(lab.snapshot_path(&loose_snapshot).exists());

    let applied = String::from_utf8(lab.rewind(&["gc", "--yes"])?.stdout)?;
    assert!(applied.contains("garbage collection complete"));
    assert!(!lab.object_path(&loose_object).exists());
    assert!(!lab.snapshot_path(&loose_snapshot).exists());
    assert!(lab.object_path(&reachable_object).exists());
    assert!(lab.snapshot_path(&reachable_snapshot).exists());
    Ok(())
}

#[test]
fn gc_yes_refuses_when_reachable_storage_has_errors() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo keep > notes.txt"])?;
    let (_, reachable_snapshot) = lab.latest_event_snapshots()?;
    let (reachable_object, _) = lab.first_file_hash_and_size(&reachable_snapshot)?;
    fs::remove_file(lab.object_path(&reachable_object))?;

    let output = lab.rewind_raw(&["gc", "--yes"])?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(!output.status.success());
    assert!(stdout.contains("refusing garbage collection"));
    assert!(lab.snapshot_path(&reachable_snapshot).exists());
    Ok(())
}

#[test]
fn gc_yes_removes_empty_object_directories_when_safe() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let hash = sha256_hex(b"nested loose object\n");
    let nested_dir = lab.path().join(".rewind/objects/aa/bb");
    fs::create_dir_all(&nested_dir)?;
    fs::write(nested_dir.join(&hash), b"nested loose object\n")?;

    lab.rewind(&["gc", "--yes"])?;

    assert!(!nested_dir.exists());
    Ok(())
}

#[test]
fn checkout_snapshot_unique_prefix_works_and_bad_prefixes_fail_clearly() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    let (_, v1_snapshot) = lab.latest_event_snapshots()?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    let prefix = &v1_snapshot[..8];
    lab.rewind(&["checkout", "--snapshot", prefix])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v1\n");

    fs::write(
        lab.snapshot_path("abc111"),
        r#"{"id":"abc111","created_at":"now","directories":[],"files":{}}"#,
    )?;
    fs::write(
        lab.snapshot_path("abc222"),
        r#"{"id":"abc222","created_at":"now","directories":[],"files":{}}"#,
    )?;

    let ambiguous = lab.rewind_raw(&["checkout", "--snapshot", "abc"])?;
    let ambiguous_stdout = String::from_utf8_lossy(&ambiguous.stdout);
    let ambiguous_stderr = String::from_utf8_lossy(&ambiguous.stderr);
    assert!(!ambiguous.status.success());
    assert!(
        ambiguous_stdout.contains("Ambiguous snapshot prefix abc")
            || ambiguous_stderr.contains("Ambiguous snapshot prefix abc")
    );
    assert!(ambiguous_stdout.contains("abc111") || ambiguous_stderr.contains("abc111"));
    assert!(ambiguous_stdout.contains("abc222") || ambiguous_stderr.contains("abc222"));

    let unknown = lab.rewind_raw(&["checkout", "--snapshot", "does-not-exist"])?;
    let unknown_stdout = String::from_utf8_lossy(&unknown.stdout);
    let unknown_stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(!unknown.status.success());
    assert!(
        unknown_stdout.contains("No snapshot matches prefix does-not-exist")
            || unknown_stderr.contains("No snapshot matches prefix does-not-exist")
    );
    Ok(())
}

#[test]
fn tui_once_clean_initialized_workspace_displays_head_status_stats_and_help() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let stdout = String::from_utf8(lab.rewind(&["tui", "--once"])?.stdout)?;

    assert!(stdout.contains("HEAD:"));
    assert!(stdout.contains("Worktree: clean"));
    assert!(stdout.contains("Timeline:"));
    assert!(stdout.contains("Stats:"));
    assert!(stdout.contains("events: 0"));
    assert!(stdout.contains("checkpoints: 0"));
    assert!(stdout.contains("Help:"));
    assert!(stdout.contains("q/Esc: quit"));
    Ok(())
}

#[test]
fn tui_once_displays_dirty_worktree_status_without_writing_storage() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo tracked > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    fs::write(lab.path().join("notes.txt"), "manual\n")?;
    fs::write(lab.path().join("scratch.txt"), "scratch\n")?;

    let stdout = String::from_utf8(lab.rewind(&["tui", "--once"])?.stdout)?;

    assert!(stdout.contains("Worktree: dirty"));
    assert!(stdout.contains("+1"));
    assert!(stdout.contains("~1"));
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    Ok(())
}

#[test]
fn tui_once_selected_event_displays_details_diff_and_suggested_commands() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    let stdout = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "2"])?.stdout)?;

    assert!(stdout.contains("> 2 run active"));
    assert!(stdout.contains("Selected Event:"));
    assert!(stdout.contains("ID: 2"));
    assert!(stdout.contains("Kind: run"));
    assert!(stdout.contains("Command: sh -c echo bad > notes.txt"));
    assert!(stdout.contains("Before:"));
    assert!(stdout.contains("After:"));
    assert!(stdout.contains("Modified:"));
    assert!(stdout.contains("notes.txt"));
    assert!(stdout.contains("--- notes.txt before event 2"));
    assert!(stdout.contains("+++ notes.txt after event 2"));
    assert!(stdout.contains("-good"));
    assert!(stdout.contains("+bad"));
    assert!(stdout.contains("Suggested commands:"));
    assert!(stdout.contains("rewind show 2"));
    assert!(stdout.contains("rewind diff 2"));
    assert!(stdout.contains("rewind checkout --before 2 --dry-run"));
    assert!(stdout.contains("rewind checkout --after 2 --dry-run"));
    Ok(())
}

#[test]
fn tui_once_displays_created_modified_and_deleted_files_for_selected_events() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo one > notes.txt"])?;
    lab.run(&["sh", "-c", "echo two > notes.txt"])?;
    lab.run(&["rm", "notes.txt"])?;

    let created = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "1"])?.stdout)?;
    let modified = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "2"])?.stdout)?;
    let deleted = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "3"])?.stdout)?;

    assert!(created.contains("Created:"));
    assert!(created.contains("notes.txt"));
    assert!(modified.contains("Modified:"));
    assert!(modified.contains("notes.txt"));
    assert!(deleted.contains("Deleted:"));
    assert!(deleted.contains("notes.txt"));
    Ok(())
}

#[test]
fn tui_once_displays_checkpoint_information_and_stats_summary() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.rewind(&[
        "checkpoint",
        "create",
        "before-bad",
        "-m",
        "Before bad edit",
    ])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    let stdout = String::from_utf8(lab.rewind(&["tui", "--once"])?.stdout)?;

    assert!(stdout.contains("Checkpoints:"));
    assert!(stdout.contains("before-bad"));
    assert!(stdout.contains("Before bad edit"));
    assert!(stdout.contains("rewind checkpoint show before-bad"));
    assert!(stdout.contains("rewind checkout --checkpoint before-bad --dry-run"));
    assert!(stdout.contains("reachable snapshots:"));
    assert!(stdout.contains("reachable objects:"));
    assert!(stdout.contains("reclaimable bytes:"));
    Ok(())
}

#[test]
fn tui_once_missing_selected_event_fails_clearly() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind_raw(&["tui", "--once", "--selected", "99"])?;
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success());
    assert!(stderr.contains("event 99 not found"));
    Ok(())
}

#[test]
fn tui_once_is_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    lab.rewind(&["tui", "--once", "--selected", "1"])?;

    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    Ok(())
}

#[test]
fn rewind_directory_never_appears_in_tui_output() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewind/internal.txt"), "ignore me")?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;

    let stdout = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "1"])?.stdout)?;

    assert!(!stdout.contains(".rewind"));
    Ok(())
}

#[test]
fn recover_status_reports_no_active_transaction_and_is_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;

    let status = String::from_utf8(lab.rewind(&["recover", "--status"])?.stdout)?;
    let default = String::from_utf8(lab.rewind(&["recover"])?.stdout)?;

    assert!(status.contains("No active Rewind transaction."));
    assert!(default.contains("No active Rewind transaction."));
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    Ok(())
}

#[test]
fn successful_restore_style_operations_archive_or_remove_active_journal() -> Result<()> {
    let undo_lab = Lab::new();
    undo_lab.init()?;
    undo_lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    undo_lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    undo_lab.rewind(&["undo"])?;
    assert!(!undo_lab.active_journal_path().exists());
    assert!(undo_lab.completed_journal_count()? >= 1);

    let restore_lab = Lab::new();
    restore_lab.init()?;
    restore_lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    restore_lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    restore_lab.rewind(&["restore", "notes.txt", "--before", "2"])?;
    assert!(!restore_lab.active_journal_path().exists());
    assert!(restore_lab.completed_journal_count()? >= 1);

    let checkout_lab = Lab::new();
    checkout_lab.init()?;
    checkout_lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    checkout_lab.rewind(&["checkpoint", "create", "v1", "-m", "v1"])?;
    checkout_lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    checkout_lab.rewind(&["checkout", "--checkpoint", "v1"])?;
    assert!(!checkout_lab.active_journal_path().exists());
    assert!(checkout_lab.completed_journal_count()? >= 1);
    Ok(())
}

#[test]
fn active_journal_blocks_mutating_commands_and_is_reported_by_verify_stats_tui() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    let stopped = lab.rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?;
    assert!(!stopped.status.success());
    assert!(lab.active_journal_path().exists());

    for args in [
        vec!["run", "--", "sh", "-c", "echo nope > nope.txt"],
        vec!["commit", "-m", "nope"],
        vec!["undo"],
        vec!["restore", "notes.txt", "--before", "2"],
        vec!["checkout", "--after", "2"],
        vec!["checkpoint", "create", "blocked", "-m", "blocked"],
        vec!["checkpoint", "delete", "blocked"],
        vec!["gc", "--yes"],
    ] {
        let output = lab.rewind_raw(&args)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!output.status.success(), "{args:?} should fail");
        assert!(
            stderr.contains("active Rewind transaction")
                || stdout.contains("active Rewind transaction"),
            "{args:?} did not mention active transaction"
        );
    }

    let verify = String::from_utf8(lab.rewind(&["verify"])?.stdout)?;
    assert!(verify.contains("Active journal"));
    assert!(!lab.rewind_raw(&["verify", "--strict"])?.status.success());
    let stats = String::from_utf8(lab.rewind(&["stats"])?.stdout)?;
    assert!(stats.contains("active journal: yes"));
    let tui = String::from_utf8(lab.rewind(&["tui", "--once"])?.stdout)?;
    assert!(tui.contains("Recovery: active transaction"));
    assert!(tui.contains("rewind recover --status"));
    Ok(())
}

#[test]
fn recover_complete_finishes_interrupted_checkout_and_is_idempotent() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    let events_before = lab.event_count()?;

    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    let status = String::from_utf8(lab.rewind(&["recover", "--status"])?.stdout)?;
    assert!(status.contains("Operation: checkout"));
    assert!(status.contains("rewind recover --complete"));

    lab.rewind(&["recover", "--complete"])?;
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "v1\n");
    assert_eq!(lab.event_count()?, events_before + 1);
    assert!(!lab.active_journal_path().exists());
    assert!(lab.rewind(&["recover", "--complete"]).is_ok());
    assert_eq!(lab.event_count()?, events_before + 1);
    Ok(())
}

#[test]
fn recover_complete_finishes_interrupted_undo_and_targeted_restore() -> Result<()> {
    let undo_lab = Lab::new();
    undo_lab.init()?;
    undo_lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    undo_lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    assert!(!undo_lab
        .rewind_raw(&["undo", "--debug-stop-after-journal"])?
        .status
        .success());
    undo_lab.rewind(&["recover", "--complete"])?;
    assert_eq!(
        fs::read_to_string(undo_lab.path().join("notes.txt"))?,
        "v1\n"
    );
    assert_eq!(undo_lab.undone_count()?, 1);

    let restore_lab = Lab::new();
    restore_lab.init()?;
    restore_lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    restore_lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    assert!(!restore_lab
        .rewind_raw(&[
            "restore",
            "notes.txt",
            "--before",
            "2",
            "--debug-stop-after-journal",
        ])?
        .status
        .success());
    restore_lab.rewind(&["recover", "--complete"])?;
    assert_eq!(
        fs::read_to_string(restore_lab.path().join("notes.txt"))?,
        "good\n"
    );
    assert_eq!(restore_lab.latest_event_kind_and_command()?.0, "restore");
    Ok(())
}

#[test]
fn recover_abort_returns_to_old_head_before_metadata_commit() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir -p src empty && echo v1 > src/notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    lab.run(&[
        "sh",
        "-c",
        "rm -rf empty && echo v2 > src/notes.txt && echo extra > extra.txt",
    ])?;

    assert!(!lab
        .rewind_raw(&[
            "checkout",
            "--snapshot",
            &head_before,
            "--debug-stop-after-journal"
        ])?
        .status
        .success());
    lab.rewind(&["recover", "--abort"])?;

    assert_eq!(
        fs::read_to_string(lab.path().join("src/notes.txt"))?,
        "v2\n"
    );
    assert!(lab.path().join("extra.txt").exists());
    assert!(!lab.path().join("empty").exists());
    assert!(!lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn recover_abort_refuses_after_metadata_commit() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-commit"])?
        .status
        .success());

    let output = lab.rewind_raw(&["recover", "--abort"])?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("metadata is already committed"));
    lab.rewind(&["recover", "--complete"])?;
    assert!(!lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn recover_reports_corrupt_or_missing_snapshot_journals_clearly() -> Result<()> {
    let corrupt = Lab::new();
    corrupt.init()?;
    fs::create_dir_all(corrupt.path().join(".rewind/journal"))?;
    fs::write(corrupt.active_journal_path(), "{not json")?;
    let output = corrupt.rewind_raw(&["recover", "--status"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active journal"));

    let missing = Lab::new();
    missing.init()?;
    missing.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    assert!(!missing
        .rewind_raw(&["checkout", "--before", "1", "--debug-stop-after-journal"])?
        .status
        .success());
    let journal: Value = serde_json::from_str(&fs::read_to_string(missing.active_journal_path())?)?;
    let old_head = journal["old_head_snapshot"].as_str().context("old head")?;
    fs::remove_file(missing.snapshot_path(old_head))?;
    let output = missing.rewind_raw(&["recover", "--status"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("old head snapshot"));
    Ok(())
}

#[test]
fn log_file_and_directory_history_reports_changes_and_rejects_invalid_paths() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p src && echo good > notes.txt && echo lib > src/lib.rs",
    ])?;
    lab.run(&[
        "sh",
        "-c",
        "echo bad > notes.txt && echo main > src/main.rs",
    ])?;
    lab.run(&["rm", "notes.txt"])?;
    lab.rewind(&["restore", "notes.txt", "--before", "3"])?;
    lab.rewind(&["undo"])?;

    let file_log = String::from_utf8(lab.rewind(&["log", "notes.txt"])?.stdout)?;
    assert!(file_log.contains("Path history for notes.txt"));
    assert!(file_log.contains("created"));
    assert!(file_log.contains("modified"));
    assert!(file_log.contains("deleted"));
    assert!(file_log.contains("restore"));
    assert!(file_log.contains("undone"));

    let dir_log = String::from_utf8(lab.rewind(&["log", "src"])?.stdout)?;
    assert!(dir_log.contains("src/lib.rs"));
    assert!(dir_log.contains("src/main.rs"));

    let unseen = String::from_utf8(lab.rewind(&["log", "missing.txt"])?.stdout)?;
    assert!(unseen.contains("No history found for missing.txt."));

    for path in ["/absolute.txt", "../outside.txt", ".rewind/events.db"] {
        let output = lab.rewind_raw(&["log", path])?;
        assert!(!output.status.success(), "{path} should be rejected");
    }
    Ok(())
}

#[test]
fn cat_reads_historical_content_by_event_snapshot_and_checkpoint() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "mkdir docs && echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    let (_, after_two) = lab.latest_event_snapshots()?;
    lab.rewind(&["checkpoint", "create", "bad-state", "-m", "Bad state"])?;

    assert_eq!(
        String::from_utf8(lab.rewind(&["cat", "notes.txt", "--before", "2"])?.stdout)?,
        "good\n"
    );
    assert_eq!(
        String::from_utf8(lab.rewind(&["cat", "notes.txt", "--after", "2"])?.stdout)?,
        "bad\n"
    );
    assert_eq!(
        String::from_utf8(
            lab.rewind(&["cat", "notes.txt", "--snapshot", &after_two[..8]])?
                .stdout
        )?,
        "bad\n"
    );
    assert_eq!(
        String::from_utf8(
            lab.rewind(&["cat", "notes.txt", "--checkpoint", "bad-state"])?
                .stdout
        )?,
        "bad\n"
    );

    let missing = lab.rewind_raw(&["cat", "notes.txt", "--before", "1"])?;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("did not exist"));

    let directory = lab.rewind_raw(&["cat", "docs", "--after", "1"])?;
    assert!(!directory.status.success());
    assert!(String::from_utf8_lossy(&directory.stderr).contains("directory"));

    assert!(!lab.rewind_raw(&["cat", "notes.txt"])?.status.success());
    assert!(!lab
        .rewind_raw(&["cat", "notes.txt", "--before", "2", "--after", "2"])?
        .status
        .success());
    assert!(!lab
        .rewind_raw(&["cat", "../notes.txt", "--after", "2"])?
        .status
        .success());
    Ok(())
}

#[test]
fn deleted_lists_historical_files_missing_at_head_and_suggests_restore() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p src docs && echo keep > keep.txt && echo old > src/old.rs && echo doc > docs/old.md",
    ])?;
    lab.run(&["rm", "src/old.rs"])?;

    let output = String::from_utf8(lab.rewind(&["deleted"])?.stdout)?;
    assert!(output.contains("Deleted files known to Rewind:"));
    assert!(output.contains("src/old.rs"));
    assert!(output.contains("rewind restore src/old.rs --before 2"));
    assert!(!output.contains("keep.txt"));

    let filtered = String::from_utf8(lab.rewind(&["deleted", "--path", "src"])?.stdout)?;
    assert!(filtered.contains("src/old.rs"));
    assert!(!filtered.contains("docs/old.md"));

    let clean = Lab::new();
    clean.init()?;
    clean.run(&["sh", "-c", "echo keep > keep.txt"])?;
    let clean_output = String::from_utf8(clean.rewind(&["deleted"])?.stdout)?;
    assert!(clean_output.contains("No deleted files found"));
    Ok(())
}

#[test]
fn grep_searches_snapshots_checkpoints_and_history_with_limits() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "printf 'good\\nTODO one\\n' > notes.txt && printf '\\000\\001' > binary.bin",
    ])?;
    let (_, first_snapshot) = lab.latest_event_snapshots()?;
    lab.rewind(&["checkpoint", "create", "first", "-m", "First"])?;
    lab.run(&["sh", "-c", "printf 'bad\\ntodo two\\n' > notes.txt"])?;

    let snapshot = String::from_utf8(
        lab.rewind(&["grep", "TODO", "--snapshot", &first_snapshot[..8]])?
            .stdout,
    )?;
    assert!(snapshot.contains("notes.txt:2: TODO one"));
    assert!(!snapshot.contains("binary.bin"));

    let checkpoint = String::from_utf8(
        lab.rewind(&["grep", "good", "--checkpoint", "first"])?
            .stdout,
    )?;
    assert!(checkpoint.contains("notes.txt:1: good"));

    let history = String::from_utf8(
        lab.rewind(&[
            "grep",
            "todo",
            "--history",
            "--ignore-case",
            "--max-results",
            "1",
        ])?
        .stdout,
    )?;
    assert!(history.contains("notes.txt"));
    assert!(history.contains("Result limit reached"));

    assert!(!lab.rewind_raw(&["grep", "todo"])?.status.success());
    assert!(!lab
        .rewind_raw(&["grep", "todo", "--history", "--checkpoint", "first"])?
        .status
        .success());
    Ok(())
}

#[test]
fn forensic_commands_are_read_only_and_warn_with_active_journal() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    lab.rewind(&["log", "notes.txt"])?;
    lab.rewind(&["cat", "notes.txt", "--before", "2"])?;
    lab.rewind(&["deleted"])?;
    lab.rewind(&["grep", "good", "--history"])?;

    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);

    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    let output = lab.rewind_raw(&["log", "notes.txt"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    let output = lab.rewind_raw(&["grep", "good", "--history"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    Ok(())
}

#[test]
fn tui_once_includes_forensic_suggestions_for_selected_event() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo good > notes.txt"])?;
    lab.run(&["sh", "-c", "echo bad > notes.txt"])?;

    let output = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "2"])?.stdout)?;

    assert!(output.contains("rewind log notes.txt"));
    assert!(output.contains("rewind cat notes.txt --before 2"));
    assert!(output.contains("rewind cat notes.txt --after 2"));
    Ok(())
}
