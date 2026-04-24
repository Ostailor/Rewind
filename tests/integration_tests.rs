use anyhow::{anyhow, Context, Result};
use rewind_core::object_store::sha256_hex;
use rewind_core::path_safety::validate_relative_path;
use rewind_core::snapshot::{create_snapshot, write_snapshot};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeSet;
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

    fn trace_count(&self) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row("SELECT COUNT(*) FROM command_traces", [], |row| row.get(0))?)
    }

    fn insert_captured_trace(&self, event_id: i64, path: &str) -> Result<i64> {
        self.insert_trace_access(event_id, path, "openat", "read")
    }

    fn insert_trace_access(
        &self,
        event_id: i64,
        path: &str,
        operation: &str,
        access_kind: &str,
    ) -> Result<i64> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        let trace_id = match conn.query_row(
            "SELECT id FROM command_traces WHERE event_id = ?1 ORDER BY id DESC LIMIT 1",
            [event_id],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(trace_id) => trace_id,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                conn.execute(
                    "INSERT INTO command_traces (
                        event_id, tracer, status, started_at, ended_at, raw_trace_path,
                        outside_workspace_ops, parse_error
                     ) VALUES (?1, 'strace', 'captured', 'now', 'later', NULL, 2, NULL)",
                    [event_id],
                )?;
                conn.last_insert_rowid()
            }
            Err(error) => return Err(error.into()),
        };
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM trace_file_events WHERE trace_id = ?1",
            [trace_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO trace_file_events (
                trace_id, seq, timestamp, pid, operation, path, path2,
                within_workspace, result, errno, access_kind
             ) VALUES (?1, ?2, '10:00:00', 1234, ?3, ?4, NULL, 1, '3', NULL, ?5)",
            (trace_id, seq, operation, path, access_kind),
        )?;
        conn.execute(
            "INSERT INTO trace_processes (
                trace_id, pid, parent_pid, operation, executable, timestamp, result
             ) VALUES (?1, 1234, NULL, 'execve', 'sh', '10:00:00', '0')",
            [trace_id],
        )?;
        Ok(trace_id)
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

    fn event_replay_metadata(&self, event_id: i64) -> Result<(Option<String>, String)> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        Ok(conn.query_row(
            "SELECT command_argv_json, command_cwd_relative FROM events WHERE id = ?1",
            [event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    fn clear_event_argv(&self, event_id: i64) -> Result<()> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        conn.execute(
            "UPDATE events SET command_argv_json = NULL WHERE id = ?1",
            [event_id],
        )?;
        Ok(())
    }

    fn set_event_argv(&self, event_id: i64, argv: &[&str]) -> Result<()> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        let argv_json = serde_json::to_string(argv)?;
        conn.execute(
            "UPDATE events SET command_argv_json = ?1 WHERE id = ?2",
            (&argv_json, event_id),
        )?;
        Ok(())
    }

    fn set_event_cwd(&self, event_id: i64, cwd: &str) -> Result<()> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        conn.execute(
            "UPDATE events SET command_cwd_relative = ?1 WHERE id = ?2",
            (cwd, event_id),
        )?;
        Ok(())
    }

    fn insert_trace_with_raw_path(&self, event_id: i64, raw_trace_path: &str) -> Result<()> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        conn.execute(
            "INSERT INTO command_traces (
                event_id, tracer, status, started_at, ended_at, raw_trace_path,
                outside_workspace_ops, parse_error
             ) VALUES (?1, 'strace', 'captured', 'now', 'later', ?2, 0, NULL)",
            (event_id, raw_trace_path),
        )?;
        Ok(())
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

    fn repo_manifest_path(&self) -> PathBuf {
        self.path().join(".rewind/repo.json")
    }

    fn db_schema_version(&self) -> Result<Option<String>> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        match conn.query_row(
            "SELECT value FROM schema_meta WHERE key = 'db_schema_version'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            Ok(version) => Ok(Some(version)),
            Err(rusqlite::Error::SqliteFailure(_, _))
            | Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn remove_repo_format_metadata(&self) -> Result<()> {
        fs::remove_file(self.repo_manifest_path())?;
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        conn.execute("DROP TABLE IF EXISTS schema_meta", [])?;
        Ok(())
    }

    fn set_db_schema_version(&self, version: &str) -> Result<()> {
        let conn = Connection::open(self.path().join(".rewind/events.db"))?;
        conn.execute(
            "UPDATE schema_meta SET value = ?1 WHERE key = 'db_schema_version'",
            [version],
        )?;
        Ok(())
    }

    fn write_repo_manifest_value(&self, value: &Value) -> Result<()> {
        fs::write(self.repo_manifest_path(), serde_json::to_vec_pretty(value)?)?;
        Ok(())
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

fn replay_temp_dirs(event_id: i64) -> Result<BTreeSet<PathBuf>> {
    let prefix = format!("rewind-replay-{event_id}-");
    let mut dirs = BTreeSet::new();
    for entry in fs::read_dir(std::env::temp_dir())? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            dirs.insert(entry.path());
        }
    }
    Ok(dirs)
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
fn repo_format_commands_report_current_repo_and_are_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    assert!(lab.repo_manifest_path().is_file());
    assert_eq!(lab.db_schema_version()?.as_deref(), Some("1"));

    let repo_info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(repo_info.contains("Migration status:     current"));
    assert!(repo_info.contains("Format version:       2"));
    assert!(repo_info.contains("DB schema version:    1"));

    let doctor = String::from_utf8(lab.rewind(&["doctor"])?.stdout)?;
    assert!(doctor.contains("Rewind doctor: OK"));
    assert!(doctor.contains("Repo format:        current"));

    let check = String::from_utf8(lab.rewind(&["migrate", "--check"])?.stdout)?;
    assert!(check.contains("This Rewind repo is current."));

    let stats = String::from_utf8(lab.rewind(&["stats"])?.stdout)?;
    assert!(stats.contains("Repo:"));
    assert!(stats.contains("migration status: current"));
    let tui = String::from_utf8(lab.rewind(&["tui", "--once"])?.stdout)?;
    assert!(tui.contains("Repo: format 2 / schema 1 (current)"));

    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert!(!lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn legacy_repo_is_detected_migrated_and_commands_work_afterward() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    let checkpoints_before = lab.checkpoint_count()?;
    let tracked_before = fs::read_to_string(lab.path().join("notes.txt"))?;
    lab.remove_repo_format_metadata()?;

    let repo_info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(repo_info.contains("Migration status:     needs migration"));
    assert!(repo_info.contains("Suggested command:    rewind migrate"));

    let doctor = lab.rewind_raw(&["doctor"])?;
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("Suggested action:   rewind migrate"));

    let check = lab.rewind_raw(&["migrate", "--check"])?;
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("needs migration"));

    for args in [
        vec!["status"],
        vec!["history"],
        vec!["tui", "--once"],
        vec!["run", "--", "sh", "-c", "echo nope > nope.txt"],
    ] {
        let output = lab.rewind_raw(&args)?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("rewind migrate"));
    }
    assert!(!lab.path().join("nope.txt").exists());

    let migrate = String::from_utf8(lab.rewind(&["migrate"])?.stdout)?;
    assert!(migrate.contains("Migration complete."));
    assert!(lab.repo_manifest_path().is_file());
    assert_eq!(lab.db_schema_version()?.as_deref(), Some("1"));
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert_eq!(lab.checkpoint_count()?, checkpoints_before);
    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        tracked_before
    );

    let current = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(current.contains("Migration status:     current"));
    assert!(lab.rewind(&["status"])?.status.success());
    assert!(lab.rewind(&["history"])?.status.success());
    assert!(lab.rewind(&["verify"])?.status.success());

    let second = String::from_utf8(lab.rewind(&["migrate"])?.stdout)?;
    assert!(second.contains("already current"));
    Ok(())
}

#[test]
fn format_one_repo_requires_metadata_only_migration_to_format_two() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    let tracked_before = fs::read_to_string(lab.path().join("notes.txt"))?;

    let old_manifest = json!({
        "format_version": 1,
        "db_schema_version": 1,
        "repo_id": "format-one",
        "created_at": "2026-04-24T10:00:00Z",
        "created_by_version": "0.14.0",
        "last_migrated_at": "2026-04-24T10:00:00Z",
        "last_migrated_by_version": "0.14.0"
    });
    lab.write_repo_manifest_value(&old_manifest)?;

    let status = lab.rewind_raw(&["status"])?;
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("rewind migrate"));

    let check = lab.rewind_raw(&["migrate", "--check"])?;
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("needs migration"));

    let migrate = String::from_utf8(lab.rewind(&["migrate"])?.stdout)?;
    assert!(migrate.contains("Migration complete."));
    let info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(info.contains("Format version:       2"));
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        tracked_before
    );
    Ok(())
}

#[test]
fn read_only_repo_format_commands_do_not_create_missing_events_db() -> Result<()> {
    let lab = Lab::new();
    fs::create_dir_all(lab.path().join(".rewind/objects"))?;
    fs::create_dir_all(lab.path().join(".rewind/snapshots"))?;
    let db_path = lab.path().join(".rewind/events.db");
    assert!(!db_path.exists());

    let repo_info = lab.rewind_raw(&["repo-info"])?;
    assert!(repo_info.status.success());
    assert!(String::from_utf8_lossy(&repo_info.stdout).contains("needs migration"));
    assert!(!db_path.exists());

    let doctor = lab.rewind_raw(&["doctor"])?;
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("rewind migrate"));
    assert!(!db_path.exists());

    let check = lab.rewind_raw(&["migrate", "--check"])?;
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("needs migration"));
    assert!(!db_path.exists());

    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("needs migration"));
    assert!(!db_path.exists());
    Ok(())
}

#[test]
fn invalid_and_future_repo_metadata_are_rejected_clearly() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    fs::write(lab.repo_manifest_path(), "{bad json")?;
    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("invalid"));

    let status = lab.rewind_raw(&["status"])?;
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("invalid format metadata"));

    let future = json!({
        "format_version": 3,
        "db_schema_version": 1,
        "repo_id": "future",
        "created_at": "2026-04-24T10:00:00Z",
        "created_by_version": "9.9.9",
        "last_migrated_at": "2026-04-24T10:00:00Z",
        "last_migrated_by_version": "9.9.9"
    });
    lab.write_repo_manifest_value(&future)?;
    let info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(info.contains("incompatible future format"));
    let mutating = lab.rewind_raw(&["run", "--", "sh", "-c", "echo no > no.txt"])?;
    assert!(!mutating.status.success());
    assert!(String::from_utf8_lossy(&mutating.stderr).contains("newer unsupported format"));

    let empty_repo_id = json!({
        "format_version": 2,
        "db_schema_version": 1,
        "repo_id": "",
        "created_at": "2026-04-24T10:00:00Z",
        "created_by_version": "0.13.0",
        "last_migrated_at": "2026-04-24T10:00:00Z",
        "last_migrated_by_version": "0.13.0"
    });
    lab.write_repo_manifest_value(&empty_repo_id)?;
    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("repo_id"));

    let current_manifest = json!({
        "format_version": 2,
        "db_schema_version": 1,
        "repo_id": "mismatch",
        "created_at": "2026-04-24T10:00:00Z",
        "created_by_version": "0.13.0",
        "last_migrated_at": "2026-04-24T10:00:00Z",
        "last_migrated_by_version": "0.13.0"
    });
    lab.write_repo_manifest_value(&current_manifest)?;
    lab.set_db_schema_version("0")?;
    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("does not match"));
    Ok(())
}

#[test]
fn migrate_refuses_active_journal_and_info_doctor_report_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;

    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());

    let migrate = lab.rewind_raw(&["migrate"])?;
    assert!(!migrate.status.success());
    assert!(String::from_utf8_lossy(&migrate.stderr).contains("active Rewind transaction"));

    let info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(info.contains("Active journal:       yes"));

    let doctor = lab.rewind_raw(&["doctor"])?;
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("Active journal:     present"));
    Ok(())
}

#[test]
fn packaging_commands_work_outside_repo_and_report_release_metadata() -> Result<()> {
    let lab = Lab::new();

    let short_version = lab.rewind(&["--version"])?;
    assert!(String::from_utf8(short_version.stdout)?.contains("rewind 1.0.0-rc.1"));

    let version = String::from_utf8(lab.rewind(&["version"])?.stdout)?;
    assert!(version.contains("CLI version:              1.0.0-rc.1"));
    assert!(version.contains("Supported repo format:    2"));
    assert!(version.contains("Supported DB schema:      1"));
    assert!(version.contains("Git commit:"));

    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = String::from_utf8(lab.rewind(&["completions", shell])?.stdout)?;
        assert!(output.to_lowercase().contains("rewind"));
    }

    let invalid = lab.rewind_raw(&["completions", "invalid-shell"])?;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid"));

    let man = String::from_utf8(lab.rewind(&["man"])?.stdout)?;
    assert!(man.contains(".TH REWIND 1"));
    assert!(man.contains("SAFETY NOTES"));
    assert!(man.contains("SYNOPSIS"));
    assert!(man.contains("Repository format 2 and DB schema 1"));

    let env = String::from_utf8(lab.rewind(&["env"])?.stdout)?;
    assert!(env.contains("Rewind environment"));
    assert!(env.contains("detected:            no"));
    assert!(env.contains("Supported repo format: 2"));

    let help = String::from_utf8(lab.rewind(&["--help"])?.stdout)?;
    assert!(help.contains("records workspace snapshots"));
    for command in ["init", "run", "restore", "replay", "migrate", "self-test"] {
        assert!(help.contains(command), "top-level help missing {command}");
    }
    for args in [
        vec!["run", "--help"],
        vec!["commit", "--help"],
        vec!["undo", "--help"],
        vec!["restore", "--help"],
        vec!["checkout", "--help"],
        vec!["recover", "--help"],
        vec!["verify", "--help"],
        vec!["gc", "--help"],
        vec!["trace", "--help"],
        vec!["replay", "--help"],
        vec!["migrate", "--help"],
        vec!["config", "show", "--help"],
        vec!["self-test", "--help"],
    ] {
        let output = String::from_utf8(lab.rewind(&args)?.stdout)?;
        assert!(output.contains("Usage:"), "{args:?} help missing usage");
    }
    let replay_help = String::from_utf8(lab.rewind(&["replay", "--help"])?.stdout)?;
    assert!(replay_help.contains("Replay"));
    assert!(replay_help.contains("not a security sandbox"));
    Ok(())
}

#[test]
fn packaging_commands_are_read_only_inside_current_repo() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    let checkpoints_before = lab.checkpoint_count()?;

    for args in [
        vec!["version"],
        vec!["completions", "bash"],
        vec!["man"],
        vec!["env"],
    ] {
        lab.rewind(&args)?;
    }

    let env = String::from_utf8(lab.rewind(&["env"])?.stdout)?;
    assert!(env.contains("status:              current"));
    assert!(env.contains("repo format:         2"));
    assert!(env.contains("active journal:      no"));
    assert!(env.contains("ignore enabled:      yes"));

    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert_eq!(lab.checkpoint_count()?, checkpoints_before);
    assert!(!lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn env_reports_active_journal_without_mutating_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    assert!(lab.active_journal_path().exists());
    let journal_before = fs::read(lab.active_journal_path())?;

    let output = String::from_utf8(lab.rewind(&["env"])?.stdout)?;
    assert!(output.contains("active journal:      yes"));
    assert!(output.contains("status:              current"));
    assert_eq!(fs::read(lab.active_journal_path())?, journal_before);
    Ok(())
}

#[test]
fn self_test_isolated_from_calling_repo_and_keep_preserves_artifacts() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    let output = String::from_utf8(lab.rewind(&["self-test"])?.stdout)?;
    assert!(output.contains("Rewind self-test"));
    assert!(output.contains("Result: ok"));

    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert!(!lab.active_journal_path().exists());

    let kept = String::from_utf8(lab.rewind(&["self-test", "--keep"])?.stdout)?;
    let kept_path = kept
        .lines()
        .find_map(|line| line.strip_prefix("Kept temp dir: "))
        .context("kept path line")?;
    let kept_path = PathBuf::from(kept_path);
    assert!(kept_path.join(".rewind").is_dir());
    assert!(kept_path.join(".rewind/events.db").is_file());
    assert!(!kept_path.join("notes.txt").exists());
    fs::remove_dir_all(kept_path)?;
    Ok(())
}

#[test]
fn release_docs_and_examples_exist() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(root.join("CHANGELOG.md").is_file());
    assert!(root.join("docs/COMMANDS.md").is_file());
    assert!(root.join("docs/REPO_FORMAT.md").is_file());
    assert!(root.join("docs/INSTALL.md").is_file());
    assert!(root.join("docs/RELEASE.md").is_file());
    assert!(root.join("docs/SAFETY.md").is_file());
    assert!(root.join("docs/LIMITATIONS.md").is_file());
    assert!(root.join("docs/TESTING.md").is_file());
    assert!(root.join("scripts/ci-check.sh").is_file());
    assert!(root.join("scripts/run-examples.sh").is_file());
    for script in [
        "examples/basic-time-travel.sh",
        "examples/replay-demo.sh",
        "examples/ignore-demo.sh",
        "examples/recovery-demo.sh",
        "examples/provenance-demo.sh",
    ] {
        let path = root.join(script);
        assert!(path.is_file(), "{script} missing");
        let text = fs::read_to_string(&path)?;
        assert!(text.contains("mktemp"));
    }
    let readme = fs::read_to_string(root.join("README.md"))?;
    for link in [
        "docs/COMMANDS.md",
        "docs/SAFETY.md",
        "docs/LIMITATIONS.md",
        "docs/REPO_FORMAT.md",
        "docs/INSTALL.md",
        "docs/RELEASE.md",
        "docs/TESTING.md",
    ] {
        assert!(readme.contains(link), "README missing {link}");
    }
    assert!(readme.contains("rewind completions"));
    assert!(readme.contains("rewind man"));
    assert!(readme.contains("not security-safe"));
    assert!(readme.contains("raw traces may contain sensitive"));
    let changelog = fs::read_to_string(root.join("CHANGELOG.md"))?;
    assert!(changelog.contains("1.0.0-rc.1"));
    let safety = fs::read_to_string(root.join("docs/SAFETY.md"))?;
    assert!(safety.contains("Command Safety Matrix"));
    assert!(safety.contains("Mutates workspace?"));
    let limitations = fs::read_to_string(root.join("docs/LIMITATIONS.md"))?;
    assert!(limitations.contains("Unsupported special files"));
    let release = fs::read_to_string(root.join("docs/RELEASE.md"))?;
    assert!(release.contains("v1 RC Checklist"));
    let ci = fs::read_to_string(root.join("scripts/ci-check.sh"))?;
    assert!(ci.contains("cargo fmt --check"));
    assert!(ci.contains("cargo clippy --workspace --all-targets --all-features -- -D warnings"));
    assert!(ci.contains("cargo test --workspace"));
    assert!(ci.contains("cargo run -p rewind-cli -- self-test"));
    assert!(ci.contains("scripts/run-examples.sh"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn basic_example_script_runs_with_rewind_in_path() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let _guard = CARGO_RUN_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("cargo run lock poisoned");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bin_dir = tempfile::tempdir()?;
    let shim = bin_dir.path().join("rewind");
    fs::write(
        &shim,
        format!(
            "#!/usr/bin/env sh\nexec cargo run --quiet --manifest-path '{}' -p rewind-cli -- \"$@\"\n",
            root.join("Cargo.toml").display()
        ),
    )?;
    let mut permissions = fs::metadata(&shim)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&shim, permissions)?;

    let old_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{old_path}", bin_dir.path().display());
    let output = Command::new(root.join("examples/basic-time-travel.sh"))
        .env("PATH", path)
        .output()
        .context("running basic example")?;

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("notes.txt after undo"));
    Ok(())
}

#[test]
fn hardening_weird_filenames_round_trip_through_history() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::create_dir_all(lab.path().join("nested dir"))?;
    fs::write(lab.path().join("space name.txt"), "space")?;
    fs::write(lab.path().join("unicodé.txt"), "unicode")?;
    fs::write(lab.path().join("-dash.txt"), "dash")?;
    fs::write(lab.path().join("nested dir/file [1]!.txt"), "punct")?;
    lab.rewind(&["commit", "-m", "weird filenames"])?;

    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree clean."));

    let diff = String::from_utf8(lab.rewind(&["diff", "1"])?.stdout)?;
    assert!(diff.contains("space name.txt"));
    assert!(diff.contains("unicodé.txt"));
    assert!(diff.contains("-dash.txt"));
    assert!(diff.contains("nested dir/file [1]!.txt"));

    let cat = String::from_utf8(lab.rewind(&["cat", "./-dash.txt", "--after", "1"])?.stdout)?;
    assert_eq!(cat, "dash");
    let log = String::from_utf8(lab.rewind(&["log", "nested dir"])?.stdout)?;
    assert!(log.contains("file [1]!.txt"));
    let restore = String::from_utf8(
        lab.rewind(&["restore", "space name.txt", "--after", "1", "--dry-run"])?
            .stdout,
    )?;
    assert!(restore.contains("Nothing to restore.") || restore.contains("Restore plan"));
    let checkout = String::from_utf8(
        lab.rewind(&["checkout", "--after", "1", "--dry-run"])?
            .stdout,
    )?;
    assert!(checkout.contains("worktree already at") || checkout.contains("Checkout plan"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn hardening_unix_newline_filename_is_recorded_and_readable() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let name = "line\nbreak.txt";
    fs::write(lab.path().join(name), "newline-name")?;
    lab.rewind(&["commit", "-m", "newline filename"])?;

    let cat = String::from_utf8(lab.rewind(&["cat", name, "--after", "1"])?.stdout)?;
    assert_eq!(cat, "newline-name");
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree clean."));
    Ok(())
}

#[test]
fn hardening_replay_cwd_and_trace_raw_paths_reject_workspace_escape() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    lab.set_event_cwd(1, "../outside")?;
    let replay = lab.rewind_raw(&["replay", "1", "--dry-run"])?;
    assert!(!replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("command_cwd_relative"));
    lab.set_event_cwd(1, ".")?;

    lab.insert_trace_with_raw_path(1, ".rewind/traces/../events.db")?;
    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    let stdout = String::from_utf8_lossy(&verify.stdout);
    assert!(stdout.contains("raw path is outside .rewind/traces"));
    Ok(())
}

#[test]
fn hardening_journal_archive_id_cannot_escape_journal_directory() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());

    let mut journal: Value = serde_json::from_str(&fs::read_to_string(lab.active_journal_path())?)?;
    journal["id"] = json!("../../escape");
    fs::write(
        lab.active_journal_path(),
        serde_json::to_vec_pretty(&journal)?,
    )?;

    let complete = String::from_utf8(lab.rewind(&["recover", "--complete"])?.stdout)?;
    assert!(complete.contains("Rewind transaction completed."));
    assert!(!lab.path().join(".rewind/escape.json").exists());
    assert!(!lab.path().join(".rewind/journal/escape.json").exists());
    assert!(lab
        .path()
        .join(".rewind/journal/completed/escape.json")
        .exists());
    assert!(!lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn hardening_representative_read_only_commands_do_not_mutate_repo() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.insert_captured_trace(1, "notes.txt")?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    let checkpoints_before = lab.checkpoint_count()?;

    let commands = [
        vec!["version"],
        vec!["completions", "bash"],
        vec!["man"],
        vec!["env"],
        vec!["config", "show"],
        vec!["repo-info"],
        vec!["doctor"],
        vec!["migrate", "--check"],
        vec!["status"],
        vec!["status", "--ignored"],
        vec!["history"],
        vec!["timeline"],
        vec!["show", "1"],
        vec!["diff", "1"],
        vec!["verify"],
        vec!["verify", "--strict"],
        vec!["stats"],
        vec!["tui", "--once"],
        vec!["recover", "--status"],
        vec!["log", "notes.txt"],
        vec!["cat", "notes.txt", "--after", "1"],
        vec!["deleted"],
        vec!["grep", "hello", "--history"],
        vec!["trace", "1"],
        vec!["explain", "1"],
        vec!["why", "notes.txt"],
        vec!["impact", "notes.txt"],
        vec!["graph", "1"],
        vec!["replay", "1", "--dry-run"],
    ];

    for args in commands {
        let output = lab.rewind_raw(&args)?;
        assert!(
            output.status.success(),
            "command {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(lab.event_count()?, events_before, "{args:?} created event");
        assert_eq!(lab.head_snapshot()?, head_before, "{args:?} changed head");
        assert_eq!(
            lab.snapshot_manifest_count()?,
            snapshots_before,
            "{args:?} wrote snapshot"
        );
        assert_eq!(lab.object_count()?, objects_before, "{args:?} wrote object");
        assert_eq!(
            lab.checkpoint_count()?,
            checkpoints_before,
            "{args:?} changed checkpoint"
        );
        assert!(
            !lab.active_journal_path().exists(),
            "{args:?} created journal"
        );
    }
    Ok(())
}

#[test]
fn config_show_defaults_file_and_validation_are_clear_and_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    let output = String::from_utf8(lab.rewind(&["config", "show"])?.stdout)?;
    assert!(output.contains("Config source:      file"));
    assert!(output.contains("Ignore enabled:     yes"));
    assert!(lab.path().join(".rewind/config.toml").is_file());

    fs::remove_file(lab.path().join(".rewind/config.toml"))?;
    let defaults = String::from_utf8(lab.rewind(&["config", "show"])?.stdout)?;
    assert!(defaults.contains("Config source:      defaults"));
    assert!(defaults.contains("Ignore file exists: no"));

    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);

    fs::write(
        lab.path().join(".rewind/config.toml"),
        "[ignore]\nunknown = true\n",
    )?;
    let invalid = lab.rewind_raw(&["config", "show"])?;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("unsupported config key"));

    for file_value in ["/tmp/rewindignore", "../outside", ".rewind/ignore"] {
        fs::write(
            lab.path().join(".rewind/config.toml"),
            format!("[ignore]\nenabled = true\nfile = \"{file_value}\"\n"),
        )?;
        assert!(!lab.rewind_raw(&["config", "show"])?.status.success());
    }
    Ok(())
}

#[test]
fn ignore_rules_hide_noise_but_status_ignored_lists_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(
        lab.path().join(".rewindignore"),
        "# comments are ignored\ncache.dat\ntarget/\n*.tmp\nnode_modules/\n",
    )?;
    fs::write(lab.path().join("cache.dat"), "cache\n")?;
    fs::create_dir_all(lab.path().join("target/debug"))?;
    fs::write(lab.path().join("target/debug/app"), "binary\n")?;
    fs::write(lab.path().join("notes.tmp"), "tmp\n")?;
    fs::create_dir_all(lab.path().join("src/node_modules"))?;
    fs::write(lab.path().join("src/node_modules/pkg.txt"), "pkg\n")?;

    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree dirty."));
    assert!(status.contains(".rewindignore"));
    assert!(!status.contains("cache.dat"));
    assert!(!status.contains("target/debug/app"));

    let ignored = String::from_utf8(lab.rewind(&["status", "--ignored"])?.stdout)?;
    assert!(ignored.contains("Ignored:"));
    assert!(ignored.contains("cache.dat"));
    assert!(ignored.contains("target/debug/app"));
    assert!(ignored.contains("notes.tmp"));
    assert!(ignored.contains("src/node_modules/pkg.txt"));
    assert!(!ignored.contains(".rewind/events.db"));

    fs::write(lab.path().join(".rewindignore"), "!\n")?;
    let invalid = lab.rewind_raw(&["status"])?;
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("Invalid ignore rules"));

    fs::write(lab.path().join(".rewindignore"), ".rewind/\n")?;
    let hard_excluded = String::from_utf8(lab.rewind(&["status", "--ignored"])?.stdout)?;
    assert!(!hard_excluded.contains(".rewind/events.db"));
    Ok(())
}

#[test]
fn verify_doctor_and_repo_info_report_config_and_ignore_state() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewindignore"), "target/\n*.tmp\n")?;

    let verify = String::from_utf8(lab.rewind(&["verify"])?.stdout)?;
    assert!(verify.contains("Rewind verify: OK"));
    assert!(verify.contains("Config:"));

    let doctor = String::from_utf8(lab.rewind(&["doctor"])?.stdout)?;
    assert!(doctor.contains("Rewind doctor: OK"));
    assert!(doctor.contains("Config:             OK"));

    let info = String::from_utf8(lab.rewind(&["repo-info"])?.stdout)?;
    assert!(info.contains("Config source:"));
    assert!(info.contains("file"));
    assert!(info.contains("Ignore enabled:"));
    assert!(info.contains("yes"));
    assert!(info.contains("Ignore file exists:"));

    fs::write(lab.path().join(".rewindignore"), "!\n")?;
    let verify = lab.rewind_raw(&["verify"])?;
    assert!(!verify.status.success());
    assert!(String::from_utf8_lossy(&verify.stdout).contains("Invalid config/ignore rules"));

    let doctor = lab.rewind_raw(&["doctor"])?;
    assert!(!doctor.status.success());
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("Config:             invalid"));
    Ok(())
}

#[test]
fn ignored_untracked_paths_are_omitted_from_run_and_commit_snapshots() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewindignore"), "target/\n*.tmp\n")?;
    lab.rewind(&["commit", "-m", "add ignore rules"])?;
    fs::create_dir_all(lab.path().join("target"))?;
    fs::write(lab.path().join("target/output.log"), "build\n")?;
    fs::write(lab.path().join("scratch.tmp"), "tmp\n")?;

    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree clean."));

    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let (_, after_run) = lab.latest_event_snapshots()?;
    let after_json = lab.load_snapshot_json(&after_run)?;
    assert!(after_json["files"]["notes.txt"].is_object());
    assert!(after_json["files"]["target/output.log"].is_null());
    assert!(after_json["files"]["scratch.tmp"].is_null());

    fs::write(lab.path().join("manual.txt"), "manual\n")?;
    lab.rewind(&["commit", "-m", "manual"])?;
    let (_, after_commit) = lab.latest_event_snapshots()?;
    let after_json = lab.load_snapshot_json(&after_commit)?;
    assert!(after_json["files"]["manual.txt"].is_object());
    assert!(after_json["files"]["target/output.log"].is_null());
    assert!(after_json["files"]["scratch.tmp"].is_null());
    Ok(())
}

#[test]
fn tracked_paths_that_become_ignored_are_carried_forward_without_fake_deletions() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "mkdir -p logs && echo original > keep.log && echo nested > logs/keep.log",
    ])?;

    fs::write(lab.path().join(".rewindignore"), "*.log\n")?;
    lab.rewind(&["commit", "-m", "ignore keep log"])?;
    let commit_output = String::from_utf8(lab.rewind(&["show", "2"])?.stdout)?;
    assert!(commit_output.contains(".rewindignore"));
    assert!(!commit_output.contains("keep.log"));

    let carried = String::from_utf8(lab.rewind(&["cat", "keep.log", "--after", "2"])?.stdout)?;
    assert_eq!(carried, "original\n");
    let nested = String::from_utf8(
        lab.rewind(&["cat", "logs/keep.log", "--after", "2"])?
            .stdout,
    )?;
    assert_eq!(nested, "nested\n");

    fs::write(lab.path().join("keep.log"), "changed while ignored\n")?;
    fs::write(
        lab.path().join("logs/keep.log"),
        "nested changed while ignored\n",
    )?;
    let clean = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(clean.contains("Rewind worktree clean."));

    let log = String::from_utf8(lab.rewind(&["log", "keep.log"])?.stdout)?;
    assert!(log.contains("created"));
    let dry_restore = String::from_utf8(
        lab.rewind(&["restore", "keep.log", "--after", "1", "--dry-run"])?
            .stdout,
    )?;
    assert!(dry_restore.contains("Nothing to restore") || dry_restore.contains("Restore plan"));

    fs::write(lab.path().join(".rewindignore"), "\n")?;
    let dirty = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(dirty.contains("Modified:"));
    assert!(dirty.contains("keep.log"));
    assert!(dirty.contains("logs/keep.log"));
    Ok(())
}

#[test]
fn ignored_noise_does_not_block_run_commit_undo_or_checkout_preflights() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join(".rewindignore"), "target/\n")?;
    lab.rewind(&["commit", "-m", "ignore target"])?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    fs::create_dir_all(lab.path().join("target"))?;
    fs::write(lab.path().join("target/noise.log"), "noise\n")?;

    let run = lab.rewind_raw(&["run", "--", "sh", "-c", "echo ok > ok.txt"])?;
    assert!(run.status.success());

    fs::write(lab.path().join("manual.txt"), "manual\n")?;
    let commit = lab.rewind_raw(&["commit", "-m", "manual"])?;
    assert!(commit.status.success());

    fs::write(lab.path().join("target/more.log"), "more\n")?;
    let undo = lab.rewind_raw(&["undo", "--dry-run"])?;
    assert!(undo.status.success());

    let checkout = lab.rewind_raw(&["checkout", "--before", "2", "--dry-run"])?;
    assert!(checkout.status.success());
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlinks_and_executable_bits_are_snapshotted_diffed_and_undoable() -> Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "printf '#!/bin/sh\\necho hello\\n' > build.sh && chmod +x build.sh && ln -s build.sh latest-build",
    ])?;
    let (_, after_create) = lab.latest_event_snapshots()?;
    let snapshot = lab.load_snapshot_json(&after_create)?;
    assert_eq!(snapshot["manifest_version"], 2);
    assert_eq!(snapshot["files"]["build.sh"]["executable"], true);
    assert_eq!(snapshot["symlinks"]["latest-build"]["target"], "build.sh");

    fs::remove_file(lab.path().join("latest-build"))?;
    symlink("missing-target", lab.path().join("latest-build"))?;
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Modified:"));
    assert!(status.contains("latest-build"));
    lab.rewind(&["commit", "-m", "change symlink"])?;
    let diff = String::from_utf8(lab.rewind(&["diff", "2"])?.stdout)?;
    assert!(diff.contains("Symlink changes"));
    assert!(diff.contains("build.sh -> missing-target"));

    lab.rewind(&["undo"])?;
    assert_eq!(
        fs::read_link(lab.path().join("latest-build"))?,
        PathBuf::from("build.sh")
    );

    let mut permissions = fs::metadata(lab.path().join("build.sh"))?.permissions();
    permissions.set_mode(permissions.mode() & !0o111);
    fs::set_permissions(lab.path().join("build.sh"), permissions)?;
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Modified:"));
    assert!(status.contains("build.sh"));
    lab.rewind(&["commit", "-m", "drop executable"])?;
    let diff = String::from_utf8(lab.rewind(&["diff", "3"])?.stdout)?;
    assert!(diff.contains("Mode changes"));
    assert!(diff.contains("executable yes -> no"));
    lab.rewind(&["undo"])?;
    assert!(
        fs::metadata(lab.path().join("build.sh"))?
            .permissions()
            .mode()
            & 0o111
            != 0
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_scans_do_not_follow_targets_and_ignore_uses_link_path() -> Result<()> {
    use std::os::unix::fs::symlink;

    let lab = Lab::new();
    lab.init()?;
    let outside = tempfile::tempdir()?;
    fs::write(outside.path().join("secret.txt"), "outside\n")?;
    symlink(outside.path(), lab.path().join("outside-link"))?;

    lab.rewind(&["commit", "-m", "record symlink"])?;
    let (_, after) = lab.latest_event_snapshots()?;
    let snapshot = lab.load_snapshot_json(&after)?;
    assert!(snapshot["symlinks"]["outside-link"]["target"]
        .as_str()
        .is_some_and(|target| target.contains(outside.path().to_string_lossy().as_ref())));
    assert!(snapshot["files"]["outside-link/secret.txt"].is_null());

    fs::write(lab.path().join(".rewindignore"), "ignored-link\n")?;
    lab.rewind(&["commit", "-m", "ignore link path"])?;
    symlink("build-output", lab.path().join("ignored-link"))?;
    let status = String::from_utf8(lab.rewind(&["status"])?.stdout)?;
    assert!(status.contains("Rewind worktree clean."));
    let ignored = String::from_utf8(lab.rewind(&["status", "--ignored"])?.stdout)?;
    assert!(ignored.contains("ignored-link"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn replay_materializes_and_compares_symlinks() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&[
        "sh",
        "-c",
        "echo hello > target.txt && ln -s target.txt link.txt",
    ])?;
    let output = String::from_utf8(lab.rewind(&["replay", "1", "--compare"])?.stdout)?;
    assert!(output.contains("Filesystem match: yes"));
    assert!(output.contains("exact reproduction: yes"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn checkout_and_recovery_replace_symlink_with_directory_safely() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "ln -s missing tree"])?;
    lab.run(&[
        "sh",
        "-c",
        "rm tree && mkdir tree && echo data > tree/file.txt",
    ])?;

    lab.rewind(&["checkout", "--before", "2"])?;
    assert_eq!(
        fs::read_link(lab.path().join("tree"))?,
        PathBuf::from("missing")
    );

    lab.rewind(&["checkout", "--after", "2"])?;
    assert_eq!(
        fs::read_to_string(lab.path().join("tree/file.txt"))?,
        "data\n"
    );

    lab.rewind(&["checkout", "--before", "2"])?;
    let stopped = lab.rewind_raw(&["checkout", "--after", "2", "--debug-stop-after-journal"])?;
    assert!(!stopped.status.success());
    assert!(lab.active_journal_path().exists());
    lab.rewind(&["recover", "--complete"])?;
    assert_eq!(
        fs::read_to_string(lab.path().join("tree/file.txt"))?,
        "data\n"
    );
    assert!(!lab.active_journal_path().exists());
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

#[test]
fn run_without_trace_and_trace_off_create_no_trace_metadata() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    lab.run(&["sh", "-c", "echo normal > normal.txt"])?;
    assert_eq!(lab.trace_count()?, 0);

    lab.rewind(&["run", "--trace=off", "--", "sh", "-c", "echo off > off.txt"])?;
    assert_eq!(lab.trace_count()?, 0);
    Ok(())
}

#[test]
fn trace_auto_succeeds_without_requiring_host_strace() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    lab.rewind(&[
        "run",
        "--trace=auto",
        "--",
        "sh",
        "-c",
        "echo traced > traced.txt",
    ])?;

    assert_eq!(lab.event_count()?, 1);
    let conn = Connection::open(lab.path().join(".rewind/events.db"))?;
    let status: String = conn.query_row(
        "SELECT status FROM command_traces WHERE event_id = 1",
        [],
        |row| row.get(0),
    )?;
    assert!(status == "captured" || status == "unavailable" || status == "parse_error");
    Ok(())
}

#[test]
fn trace_strace_requires_available_strace_or_captures() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;

    let output = lab.rewind_raw(&[
        "run",
        "--trace=strace",
        "--",
        "sh",
        "-c",
        "echo maybe > maybe.txt",
    ])?;

    if output.status.success() {
        assert_eq!(lab.event_count()?, 1);
        assert!(lab.trace_count()? >= 1);
    } else {
        assert_eq!(lab.event_count()?, 0);
        assert!(!lab.path().join("maybe.txt").exists());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("strace"));
    }
    Ok(())
}

#[test]
fn trace_command_show_history_timeline_stats_and_tui_display_trace_metadata() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.insert_captured_trace(1, "notes.txt")?;

    let trace = String::from_utf8(lab.rewind(&["trace", "1"])?.stdout)?;
    assert!(trace.contains("Trace for event 1"));
    assert!(trace.contains("Status:  captured"));
    assert!(trace.contains("notes.txt"));
    assert!(trace.contains("execve"));

    let files = String::from_utf8(lab.rewind(&["trace", "1", "--files"])?.stdout)?;
    assert!(files.contains("Workspace file operations"));
    assert!(files.contains("notes.txt"));
    assert!(!files.contains("Processes:"));

    let processes = String::from_utf8(lab.rewind(&["trace", "1", "--processes"])?.stdout)?;
    assert!(processes.contains("Processes:"));
    assert!(processes.contains("execve"));
    assert!(!processes.contains("Workspace file operations"));

    let show = String::from_utf8(lab.rewind(&["show", "1"])?.stdout)?;
    assert!(show.contains("Trace:"));
    assert!(show.contains("status: captured"));
    assert!(show.contains("rewind trace 1"));

    let history = String::from_utf8(lab.rewind(&["history"])?.stdout)?;
    assert!(history.contains("captured"));
    let timeline = String::from_utf8(lab.rewind(&["timeline"])?.stdout)?;
    assert!(timeline.contains("captured"));
    let stats = String::from_utf8(lab.rewind(&["stats"])?.stdout)?;
    assert!(stats.contains("Traces:"));
    assert!(stats.contains("captured:"));
    let tui = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "1"])?.stdout)?;
    assert!(tui.contains("Trace: captured"));
    assert!(tui.contains("rewind trace 1"));
    Ok(())
}

#[test]
fn trace_reports_no_trace_and_log_include_trace_shows_trace_only_touches() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let none = String::from_utf8(lab.rewind(&["trace", "1"])?.stdout)?;
    assert!(none.contains("No trace recorded for event 1."));

    lab.insert_captured_trace(1, "notes.txt")?;
    let log = String::from_utf8(lab.rewind(&["log", "notes.txt", "--include-trace"])?.stdout)?;
    assert!(log.contains("created"));
    assert!(log.contains("touched"));
    assert!(log.contains("trace"));
    Ok(())
}

#[test]
fn verify_and_active_journal_behavior_include_traces() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    lab.insert_captured_trace(1, "notes.txt")?;

    let verify = String::from_utf8(lab.rewind(&["verify"])?.stdout)?;
    assert!(verify.contains("Rewind verify: OK"));

    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    let blocked = lab.rewind_raw(&["run", "--trace=auto", "--", "sh", "-c", "echo no > no.txt"])?;
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("active Rewind transaction"));

    let trace = lab.rewind_raw(&["trace", "1"])?;
    assert!(trace.status.success());
    assert!(String::from_utf8_lossy(&trace.stderr).contains("active recovery transaction"));
    Ok(())
}

#[test]
fn explain_why_impact_and_graph_use_trace_and_final_changes() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo config > config.toml"])?;
    lab.run(&[
        "sh",
        "-c",
        "cat config.toml >/dev/null && echo hello > notes.txt",
    ])?;
    lab.insert_trace_access(2, "config.toml", "openat", "read")?;
    lab.insert_trace_access(2, "notes.txt", "openat", "write")?;

    let explain = String::from_utf8(lab.rewind(&["explain", "2"])?.stdout)?;
    assert!(explain.contains("Event 2 explanation"));
    assert!(explain.contains("Created:"));
    assert!(explain.contains("notes.txt"));
    assert!(explain.contains("Trace:"));
    assert!(explain.contains("Read:"));
    assert!(explain.contains("config.toml"));
    assert!(explain.contains("Changed and traced:"));
    assert!(explain.contains("Traced but unchanged:"));

    let why = String::from_utf8(lab.rewind(&["why", "notes.txt"])?.stdout)?;
    assert!(why.contains("Why notes.txt?"));
    assert!(why.contains("Current state: file at HEAD"));
    assert!(why.contains("Last changed by event 2"));
    assert!(why.contains("Trace:"));
    assert!(why.contains("written"));

    let impact = String::from_utf8(lab.rewind(&["impact", "config.toml"])?.stdout)?;
    assert!(impact.contains("Trace-based impact for config.toml"));
    assert!(impact.contains("read"));
    assert!(impact.contains("Trace-based results only"));

    let graph = String::from_utf8(lab.rewind(&["graph", "2"])?.stdout)?;
    assert!(graph.contains("Event 2:"));
    assert!(graph.contains("Processes"));
    assert!(graph.contains("Inputs / reads"));
    assert!(graph.contains("Outputs / final changes"));

    let dot = String::from_utf8(lab.rewind(&["graph", "2", "--dot"])?.stdout)?;
    assert!(dot.starts_with("digraph"));
    assert!(dot.contains("event_2"));
    assert!(!dot.contains("/etc/"));
    Ok(())
}

#[test]
fn provenance_commands_handle_missing_data_invalid_paths_and_are_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    let explain = String::from_utf8(lab.rewind(&["explain", "1"])?.stdout)?;
    assert!(explain.contains("Trace: missing"));
    assert!(explain.contains("Changed but not traced:"));

    let unseen = String::from_utf8(lab.rewind(&["why", "missing.txt"])?.stdout)?;
    assert!(unseen.contains("No event explains missing.txt."));

    assert!(!lab.rewind_raw(&["explain", "99"])?.status.success());
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);

    lab.run(&["rm", "notes.txt"])?;
    let deleted = String::from_utf8(lab.rewind(&["why", "notes.txt"])?.stdout)?;
    assert!(deleted.contains("Current state: missing at HEAD"));
    assert!(deleted.contains("Last known deletion"));

    for path in ["/absolute.txt", "../outside.txt", ".rewind/events.db"] {
        assert!(!lab.rewind_raw(&["why", path])?.status.success());
        assert!(!lab.rewind_raw(&["impact", path])?.status.success());
    }

    assert_eq!(lab.event_count()?, events_before + 1);
    assert_ne!(lab.head_snapshot()?, head_before);
    assert!(lab.object_count()? >= objects_before);
    Ok(())
}

#[test]
fn provenance_commands_warn_with_active_journal_and_tui_suggests_them() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo v1 > notes.txt"])?;
    lab.run(&["sh", "-c", "echo v2 > notes.txt"])?;
    lab.insert_trace_access(2, "notes.txt", "openat", "write")?;

    let tui = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "2"])?.stdout)?;
    assert!(tui.contains("rewind explain 2"));
    assert!(tui.contains("rewind graph 2"));
    assert!(tui.contains("rewind graph 2 --dot"));
    assert!(tui.contains("rewind why notes.txt"));
    assert!(tui.contains("rewind impact notes.txt"));

    let show = String::from_utf8(lab.rewind(&["show", "2"])?.stdout)?;
    assert!(show.contains("rewind explain 2"));
    let trace = String::from_utf8(lab.rewind(&["trace", "2"])?.stdout)?;
    assert!(trace.contains("rewind explain 2"));
    let log = String::from_utf8(lab.rewind(&["log", "notes.txt"])?.stdout)?;
    assert!(log.contains("rewind why notes.txt"));
    assert!(log.contains("rewind impact notes.txt"));

    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    let output = lab.rewind_raw(&["explain", "2"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    let output = lab.rewind_raw(&["why", "notes.txt"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    let output = lab.rewind_raw(&["impact", "notes.txt"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    let output = lab.rewind_raw(&["graph", "2"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    Ok(())
}

#[test]
fn run_captures_replay_metadata_and_verify_validates_it() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let (argv_json, cwd) = lab.event_replay_metadata(1)?;
    let argv: Vec<String> = serde_json::from_str(&argv_json.context("argv json")?)?;
    assert_eq!(argv, vec!["sh", "-c", "echo hello > notes.txt"]);
    assert_eq!(cwd, ".");
    assert!(lab.rewind(&["verify"])?.status.success());

    let conn = Connection::open(lab.path().join(".rewind/events.db"))?;
    conn.execute(
        "UPDATE events SET command_argv_json = '{bad json' WHERE id = 1",
        [],
    )?;
    assert!(!lab.rewind_raw(&["verify"])?.status.success());
    conn.execute("UPDATE events SET command_argv_json = NULL, command_cwd_relative = '../outside' WHERE id = 1", [])?;
    assert!(!lab.rewind_raw(&["verify"])?.status.success());
    conn.execute(
        "UPDATE events SET command_cwd_relative = '.rewind' WHERE id = 1",
        [],
    )?;
    assert!(!lab.rewind_raw(&["verify"])?.status.success());
    Ok(())
}

#[test]
fn replay_dry_run_is_default_and_read_only() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let events_before = lab.event_count()?;
    let head_before = lab.head_snapshot()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    let default = String::from_utf8(lab.rewind(&["replay", "1"])?.stdout)?;
    let dry_run = String::from_utf8(lab.rewind(&["replay", "1", "--dry-run"])?.stdout)?;

    assert!(default.contains("Replay plan for event 1"));
    assert!(dry_run.contains("Replay source: argv"));
    assert!(dry_run.contains("restore snapshot"));
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert!(!lab.path().join(".rewind/journal/active.json").exists());
    Ok(())
}

#[test]
fn replay_refuses_non_run_events() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    fs::write(lab.path().join("manual.txt"), "manual\n")?;
    lab.rewind(&["commit", "-m", "manual"])?;

    let output = lab.rewind_raw(&["replay", "1", "--dry-run"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only supports run events"));
    Ok(())
}

#[test]
fn replay_sandbox_and_compare_reproduce_simple_events_without_mutating_workspace() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;
    let checkpoints_before = lab.checkpoint_count()?;

    let sandbox = String::from_utf8(lab.rewind(&["replay", "1", "--sandbox"])?.stdout)?;
    assert!(sandbox.contains("exact reproduction: yes"));
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert_eq!(lab.checkpoint_count()?, checkpoints_before);
    assert!(!lab.path().join(".rewind/journal/active.json").exists());

    lab.run(&["sh", "-c", "echo goodbye > notes.txt"])?;
    let modify = String::from_utf8(lab.rewind(&["replay", "2", "--compare"])?.stdout)?;
    assert!(modify.contains("exact reproduction: yes"));

    lab.run(&["rm", "notes.txt"])?;
    let delete = String::from_utf8(lab.rewind(&["replay", "3", "--compare"])?.stdout)?;
    assert!(delete.contains("exact reproduction: yes"));
    Ok(())
}

#[test]
fn replay_rejects_absolute_argv_without_running_or_mutating_workspace() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo original > notes.txt"])?;
    lab.set_event_argv(1, &["/bin/sh", "-c", "echo changed > notes.txt"])?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;
    let snapshots_before = lab.snapshot_manifest_count()?;
    let objects_before = lab.object_count()?;

    let output = lab.rewind_raw(&["replay", "1", "--sandbox"])?;

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute executable path"));
    assert_eq!(
        fs::read_to_string(lab.path().join("notes.txt"))?,
        "original\n"
    );
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    assert_eq!(lab.snapshot_manifest_count()?, snapshots_before);
    assert_eq!(lab.object_count()?, objects_before);
    assert!(!lab.path().join(".rewind/journal/active.json").exists());
    Ok(())
}

#[test]
fn replay_cleans_sandbox_after_command_spawn_failure() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.set_event_argv(1, &["definitely-not-a-rewind-command"])?;
    let temp_dirs_before = replay_temp_dirs(1)?;
    let head_before = lab.head_snapshot()?;
    let events_before = lab.event_count()?;

    let output = lab.rewind_raw(&["replay", "1", "--sandbox"])?;

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("replaying event 1"));
    assert_eq!(replay_temp_dirs(1)?, temp_dirs_before);
    assert_eq!(fs::read_to_string(lab.path().join("notes.txt"))?, "hello\n");
    assert_eq!(lab.head_snapshot()?, head_before);
    assert_eq!(lab.event_count()?, events_before);
    Ok(())
}

#[test]
fn replay_compare_reports_path_dependent_content_mismatch() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "pwd > where.txt"])?;

    let output = String::from_utf8(lab.rewind(&["replay", "1", "--compare"])?.stdout)?;

    assert!(output.contains("Filesystem match: no"));
    assert!(output.contains("Content mismatches:"));
    assert!(output.contains("where.txt"));
    assert!(output.contains("--- where.txt original"));
    assert!(output.contains("+++ where.txt replay"));
    Ok(())
}

#[test]
fn replay_keep_preserves_sandbox_artifacts() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let output = String::from_utf8(lab.rewind(&["replay", "1", "--sandbox", "--keep"])?.stdout)?;
    let root_line = output
        .lines()
        .find(|line| line.trim_start().starts_with("root:"))
        .context("root line")?;
    let root = PathBuf::from(
        root_line
            .split_once(':')
            .context("root separator")?
            .1
            .trim(),
    );

    assert!(root.join("workspace/notes.txt").exists());
    assert!(root.join("stdout.txt").exists());
    assert!(root.join("stderr.txt").exists());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn replay_legacy_fallback_and_active_journal_warning_work() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;
    lab.clear_event_argv(1)?;

    let dry = lab.rewind_raw(&["replay", "1", "--dry-run"])?;
    if cfg!(unix) {
        assert!(dry.status.success());
        let stdout = String::from_utf8_lossy(&dry.stdout);
        assert!(stdout.contains("legacy-shell-fallback"));
        assert!(stdout.contains("shell fallback"));
    } else {
        assert!(!dry.status.success());
        assert!(String::from_utf8_lossy(&dry.stderr).contains("no exact argv"));
    }

    lab.run(&["sh", "-c", "echo goodbye > notes.txt"])?;
    assert!(!lab
        .rewind_raw(&["checkout", "--before", "2", "--debug-stop-after-journal"])?
        .status
        .success());
    let output = lab.rewind_raw(&["replay", "2", "--dry-run"])?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("active recovery transaction"));
    assert!(lab.active_journal_path().exists());
    Ok(())
}

#[test]
fn replay_suggestions_appear_in_show_tui_and_stats() -> Result<()> {
    let lab = Lab::new();
    lab.init()?;
    lab.run(&["sh", "-c", "echo hello > notes.txt"])?;

    let show = String::from_utf8(lab.rewind(&["show", "1"])?.stdout)?;
    assert!(show.contains("Replay:"));
    assert!(show.contains("rewind replay 1 --dry-run"));
    assert!(show.contains("rewind replay 1 --compare"));
    let tui = String::from_utf8(lab.rewind(&["tui", "--once", "--selected", "1"])?.stdout)?;
    assert!(tui.contains("rewind replay 1 --dry-run"));
    assert!(tui.contains("rewind replay 1 --compare"));
    let stats = String::from_utf8(lab.rewind(&["stats"])?.stdout)?;
    assert!(stats.contains("Replay:"));
    assert!(stats.contains("exact argv:"));
    Ok(())
}
