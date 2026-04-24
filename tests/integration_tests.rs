use anyhow::{anyhow, Context, Result};
use rewind_core::path_safety::validate_relative_path;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

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
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "hello\n")?;

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
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "hello\n")?;

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
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
    lab.run(&["sh", "-c", "echo goodbye > notes.txt"])?;

    lab.rewind(&["undo"])?;

    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    Ok(())
}

#[test]
fn undo_reverses_a_file_deletion() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("notes.txt"), "hello\n")?;
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
