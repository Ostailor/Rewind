use crate::diff::{ChangeType, FileChange, SnapshotDiff};
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: i64,
    pub kind: String,
    pub started_dirty: bool,
    pub timestamp: String,
    pub command: String,
    pub command_argv_json: Option<String>,
    pub command_cwd_relative: String,
    pub exit_code: i32,
    pub before_snapshot: String,
    pub after_snapshot: String,
    pub transaction_id: Option<String>,
    pub created_count: i64,
    pub modified_count: i64,
    pub deleted_count: i64,
    pub undone: bool,
}

pub struct NewEvent<'a> {
    pub kind: &'a str,
    pub started_dirty: bool,
    pub timestamp: &'a str,
    pub command: &'a str,
    pub command_argv_json: Option<&'a str>,
    pub command_cwd_relative: &'a str,
    pub exit_code: i32,
    pub before_snapshot: &'a str,
    pub after_snapshot: &'a str,
    pub diff: &'a SnapshotDiff,
    pub transaction_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub name: String,
    pub snapshot_id: String,
    pub message: String,
    pub created_at: String,
}

pub fn open(project_dir: &Path) -> Result<Connection> {
    let db_path = project_dir.join(REWIND_DIR).join("events.db");
    let conn =
        Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("enabling sqlite foreign keys")?;
    Ok(conn)
}

pub fn ensure_initialized(project_dir: &Path) -> Result<Connection> {
    if !project_dir.join(REWIND_DIR).is_dir() {
        bail!(
            "{} is not initialized; run `rewind init` first",
            project_dir.display()
        );
    }
    open(project_dir)
}

pub fn insert_event(conn: &mut Connection, event: NewEvent<'_>) -> Result<i64> {
    let tx = conn.transaction().context("starting history transaction")?;
    tx.execute(
        "INSERT INTO events (
            kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
            exit_code, before_snapshot, after_snapshot,
            created_count, modified_count, deleted_count, transaction_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            event.kind,
            if event.started_dirty { 1 } else { 0 },
            event.timestamp,
            event.command,
            event.command_argv_json,
            event.command_cwd_relative,
            event.exit_code,
            event.before_snapshot,
            event.after_snapshot,
            event.diff.created_count as i64,
            event.diff.modified_count as i64,
            event.diff.deleted_count as i64,
            event.transaction_id,
        ],
    )
    .context("inserting event")?;
    let event_id = tx.last_insert_rowid();

    for change in &event.diff.changes {
        tx.execute(
            "INSERT INTO file_changes (
                event_id, path, change_type, before_hash, after_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event_id,
                change.path,
                change.change_type.as_str(),
                change.before_hash,
                change.after_hash,
            ],
        )
        .with_context(|| format!("inserting file change for {}", change.path))?;
    }

    tx.commit().context("committing history transaction")?;
    Ok(event_id)
}

pub fn list_events(conn: &Connection) -> Result<Vec<Event>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
                    exit_code, before_snapshot, after_snapshot,
                    transaction_id, created_count, modified_count, deleted_count, undone
             FROM events
             ORDER BY id ASC",
        )
        .context("preparing event list query")?;
    let rows = stmt
        .query_map([], event_from_row)
        .context("querying events")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading events")
}

pub fn get_event(conn: &Connection, event_id: i64) -> Result<Option<Event>> {
    conn.query_row(
        "SELECT id, kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
                exit_code, before_snapshot, after_snapshot,
                transaction_id, created_count, modified_count, deleted_count, undone
         FROM events
         WHERE id = ?1",
        params![event_id],
        event_from_row,
    )
    .optional()
    .context("querying event")
}

pub fn list_changes(conn: &Connection, event_id: i64) -> Result<Vec<FileChange>> {
    let mut stmt = conn
        .prepare(
            "SELECT path, change_type, before_hash, after_hash
             FROM file_changes
             WHERE event_id = ?1
             ORDER BY path ASC",
        )
        .context("preparing change list query")?;
    let rows = stmt
        .query_map(params![event_id], |row| {
            let change_type: String = row.get(1)?;
            Ok(FileChange {
                path: row.get(0)?,
                change_type: match change_type.as_str() {
                    "created" => ChangeType::Created,
                    "modified" => ChangeType::Modified,
                    "deleted" => ChangeType::Deleted,
                    _ => ChangeType::Modified,
                },
                before_hash: row.get(2)?,
                after_hash: row.get(3)?,
                before_kind: None,
                after_kind: None,
            })
        })
        .context("querying file changes")?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading file changes")
}

pub fn latest_non_undone_event(conn: &Connection) -> Result<Option<Event>> {
    conn.query_row(
        "SELECT id, kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
                exit_code, before_snapshot, after_snapshot,
                transaction_id, created_count, modified_count, deleted_count, undone
         FROM events
         WHERE undone = 0
         ORDER BY id DESC
         LIMIT 1",
        [],
        event_from_row,
    )
    .optional()
    .context("querying latest non-undone event")
}

pub fn latest_non_undone_event_for_head(
    conn: &Connection,
    head_snapshot: &str,
) -> Result<Option<Event>> {
    conn.query_row(
        "SELECT id, kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
                exit_code, before_snapshot, after_snapshot,
                transaction_id, created_count, modified_count, deleted_count, undone
         FROM events
         WHERE undone = 0 AND after_snapshot = ?1
         ORDER BY id DESC
         LIMIT 1",
        params![head_snapshot],
        event_from_row,
    )
    .optional()
    .context("querying latest non-undone event for head snapshot")
}

pub fn mark_undone(conn: &Connection, event_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE events SET undone = 1 WHERE id = ?1",
        params![event_id],
    )
    .with_context(|| format!("marking event {event_id} undone"))?;
    Ok(())
}

pub fn set_workspace_state(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_state (key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .with_context(|| format!("setting workspace state {key}"))?;
    Ok(())
}

pub fn get_workspace_state(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM workspace_state WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .with_context(|| format!("reading workspace state {key}"))
}

pub fn set_head_snapshot(conn: &Connection, snapshot_id: &str) -> Result<()> {
    set_workspace_state(conn, "head_snapshot", snapshot_id)
}

pub fn get_head_snapshot(conn: &Connection) -> Result<Option<String>> {
    get_workspace_state(conn, "head_snapshot")
}

pub fn initialize_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS events (
             id INTEGER PRIMARY KEY,
             kind TEXT NOT NULL DEFAULT 'run',
             started_dirty INTEGER NOT NULL DEFAULT 0,
             timestamp TEXT NOT NULL,
             command TEXT NOT NULL,
             command_argv_json TEXT NULL,
             command_cwd_relative TEXT NOT NULL DEFAULT '.',
             exit_code INTEGER NOT NULL,
             before_snapshot TEXT NOT NULL,
             after_snapshot TEXT NOT NULL,
             transaction_id TEXT NULL,
             created_count INTEGER NOT NULL,
             modified_count INTEGER NOT NULL,
             deleted_count INTEGER NOT NULL,
             undone INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS file_changes (
             id INTEGER PRIMARY KEY,
             event_id INTEGER NOT NULL,
             path TEXT NOT NULL,
             change_type TEXT NOT NULL,
             before_hash TEXT NULL,
             after_hash TEXT NULL,
             FOREIGN KEY(event_id) REFERENCES events(id)
         );
         CREATE TABLE IF NOT EXISTS workspace_state (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS checkpoints (
             name TEXT PRIMARY KEY,
             snapshot_id TEXT NOT NULL,
             message TEXT NOT NULL,
             created_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS command_traces (
             id INTEGER PRIMARY KEY,
             event_id INTEGER NOT NULL,
             tracer TEXT NOT NULL,
             status TEXT NOT NULL,
             started_at TEXT NOT NULL,
             ended_at TEXT NULL,
             raw_trace_path TEXT NULL,
             outside_workspace_ops INTEGER NOT NULL DEFAULT 0,
             parse_error TEXT NULL
         );
         CREATE TABLE IF NOT EXISTS trace_processes (
             id INTEGER PRIMARY KEY,
             trace_id INTEGER NOT NULL,
             pid INTEGER NULL,
             parent_pid INTEGER NULL,
             operation TEXT NOT NULL,
             executable TEXT NULL,
             timestamp TEXT NULL,
             result TEXT NULL
         );
         CREATE TABLE IF NOT EXISTS trace_file_events (
             id INTEGER PRIMARY KEY,
             trace_id INTEGER NOT NULL,
             seq INTEGER NOT NULL,
             timestamp TEXT NULL,
             pid INTEGER NULL,
             operation TEXT NOT NULL,
             path TEXT NULL,
             path2 TEXT NULL,
             within_workspace INTEGER NOT NULL,
             result TEXT NULL,
             errno TEXT NULL,
             access_kind TEXT NOT NULL DEFAULT 'unknown'
         );",
    )
    .context("initializing events database schema")?;
    ensure_events_kind_column(conn)?;
    ensure_events_started_dirty_column(conn)?;
    ensure_events_transaction_id_column(conn)?;
    ensure_events_command_argv_json_column(conn)?;
    ensure_events_command_cwd_relative_column(conn)?;
    ensure_trace_file_access_kind_column(conn)?;
    Ok(())
}

fn ensure_events_kind_column(conn: &Connection) -> Result<()> {
    let columns = event_columns(conn)?;
    if !columns.iter().any(|column| column == "kind") {
        conn.execute(
            "ALTER TABLE events ADD COLUMN kind TEXT NOT NULL DEFAULT 'run'",
            [],
        )
        .context("adding events.kind column")?;
    }

    Ok(())
}

fn ensure_events_started_dirty_column(conn: &Connection) -> Result<()> {
    let columns = event_columns(conn)?;
    if !columns.iter().any(|column| column == "started_dirty") {
        conn.execute(
            "ALTER TABLE events ADD COLUMN started_dirty INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .context("adding events.started_dirty column")?;
    }

    Ok(())
}

fn ensure_events_transaction_id_column(conn: &Connection) -> Result<()> {
    let columns = event_columns(conn)?;
    if !columns.iter().any(|column| column == "transaction_id") {
        conn.execute("ALTER TABLE events ADD COLUMN transaction_id TEXT NULL", [])
            .context("adding events.transaction_id column")?;
    }

    Ok(())
}

fn ensure_events_command_argv_json_column(conn: &Connection) -> Result<()> {
    let columns = event_columns(conn)?;
    if !columns.iter().any(|column| column == "command_argv_json") {
        conn.execute(
            "ALTER TABLE events ADD COLUMN command_argv_json TEXT NULL",
            [],
        )
        .context("adding events.command_argv_json column")?;
    }

    Ok(())
}

fn ensure_events_command_cwd_relative_column(conn: &Connection) -> Result<()> {
    let columns = event_columns(conn)?;
    if !columns
        .iter()
        .any(|column| column == "command_cwd_relative")
    {
        conn.execute(
            "ALTER TABLE events ADD COLUMN command_cwd_relative TEXT NOT NULL DEFAULT '.'",
            [],
        )
        .context("adding events.command_cwd_relative column")?;
    }

    Ok(())
}

pub fn event_for_transaction(conn: &Connection, transaction_id: &str) -> Result<Option<Event>> {
    conn.query_row(
        "SELECT id, kind, started_dirty, timestamp, command, command_argv_json, command_cwd_relative,
                exit_code, before_snapshot, after_snapshot,
                transaction_id, created_count, modified_count, deleted_count, undone
         FROM events
         WHERE transaction_id = ?1
         ORDER BY id ASC
         LIMIT 1",
        params![transaction_id],
        event_from_row,
    )
    .optional()
    .context("querying event for transaction")
}

fn event_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(events)")
        .context("checking events columns")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("reading events columns")?;
    Ok(columns)
}

fn ensure_trace_file_access_kind_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(trace_file_events)")
        .context("checking trace_file_events columns")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("reading trace_file_events columns")?;
    if !columns.iter().any(|column| column == "access_kind") {
        conn.execute(
            "ALTER TABLE trace_file_events ADD COLUMN access_kind TEXT NOT NULL DEFAULT 'unknown'",
            [],
        )
        .context("adding trace_file_events.access_kind column")?;
    }
    Ok(())
}

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Event> {
    let started_dirty: i64 = row.get(2)?;
    let undone: i64 = row.get(14)?;
    Ok(Event {
        id: row.get(0)?,
        kind: row.get(1)?,
        started_dirty: started_dirty != 0,
        timestamp: row.get(3)?,
        command: row.get(4)?,
        command_argv_json: row.get(5)?,
        command_cwd_relative: row.get(6)?,
        exit_code: row.get(7)?,
        before_snapshot: row.get(8)?,
        after_snapshot: row.get(9)?,
        transaction_id: row.get(10)?,
        created_count: row.get(11)?,
        modified_count: row.get(12)?,
        deleted_count: row.get(13)?,
        undone: undone != 0,
    })
}
