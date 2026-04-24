use crate::history;
use crate::path_safety::validate_relative_path;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const OUTSIDE_WORKSPACE: &str = "<outside-workspace>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceMode {
    Off,
    Auto,
    Strace,
}

#[derive(Debug, Clone)]
pub enum TracePlan {
    Off,
    Unavailable {
        tracer: String,
        reason: String,
        started_at: String,
    },
    Strace {
        output_path: PathBuf,
        started_at: String,
    },
}

#[derive(Debug, Clone)]
pub struct ParsedTraceEvent {
    pub timestamp: Option<String>,
    pub pid: Option<i64>,
    pub operation: String,
    pub path: Option<String>,
    pub path2: Option<String>,
    pub within_workspace: bool,
    pub result: Option<String>,
    pub errno: Option<String>,
    pub executable: Option<String>,
    pub access_kind: String,
}

#[derive(Debug, Clone)]
pub struct CommandTrace {
    pub id: i64,
    pub event_id: i64,
    pub tracer: String,
    pub status: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub raw_trace_path: Option<String>,
    pub outside_workspace_ops: i64,
    pub parse_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TraceFileEvent {
    pub seq: i64,
    pub timestamp: Option<String>,
    pub pid: Option<i64>,
    pub operation: String,
    pub path: Option<String>,
    pub path2: Option<String>,
    pub within_workspace: bool,
    pub result: Option<String>,
    pub errno: Option<String>,
    pub access_kind: String,
}

#[derive(Debug, Clone)]
pub struct TraceProcessEvent {
    pub pid: Option<i64>,
    pub parent_pid: Option<i64>,
    pub operation: String,
    pub executable: Option<String>,
    pub timestamp: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TraceDetails {
    pub trace: CommandTrace,
    pub files: Vec<TraceFileEvent>,
    pub processes: Vec<TraceProcessEvent>,
}

#[derive(Debug, Clone, Default)]
pub struct TraceStats {
    pub total: usize,
    pub captured: usize,
    pub unavailable: usize,
    pub failed: usize,
    pub parse_error: usize,
    pub file_events: usize,
    pub process_events: usize,
}

pub fn parse_mode(value: &str) -> Result<TraceMode> {
    match value {
        "off" => Ok(TraceMode::Off),
        "auto" => Ok(TraceMode::Auto),
        "strace" => Ok(TraceMode::Strace),
        other => bail!("unknown trace mode {other}; use off, auto, or strace"),
    }
}

pub fn prepare(project_dir: &Path, mode: TraceMode) -> Result<TracePlan> {
    match mode {
        TraceMode::Off => Ok(TracePlan::Off),
        TraceMode::Auto if !strace_supported() => Ok(TracePlan::Unavailable {
            tracer: "strace".to_owned(),
            reason: unavailable_reason(),
            started_at: Utc::now().to_rfc3339(),
        }),
        TraceMode::Strace if !strace_supported() => bail!("{}", unavailable_reason()),
        TraceMode::Auto | TraceMode::Strace => {
            let tmp_dir = project_dir.join(REWIND_DIR).join("traces").join("tmp");
            fs::create_dir_all(&tmp_dir)
                .with_context(|| format!("creating {}", tmp_dir.display()))?;
            let output_path = tmp_dir.join(format!(
                "{}-{}.strace",
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                std::process::id()
            ));
            Ok(TracePlan::Strace {
                output_path,
                started_at: Utc::now().to_rfc3339(),
            })
        }
    }
}

pub fn strace_command(trace_output: &Path, command: &[String]) -> Command {
    let mut traced = Command::new("strace");
    traced
        .arg("-f")
        .arg("-tt")
        .arg("-e")
        .arg("trace=file,process")
        .arg("-o")
        .arg(trace_output)
        .arg("--")
        .arg(&command[0])
        .args(&command[1..]);
    traced
}

pub fn record_unavailable(
    conn: &Connection,
    event_id: i64,
    tracer: &str,
    reason: &str,
    started_at: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO command_traces (
            event_id, tracer, status, started_at, ended_at, raw_trace_path,
            outside_workspace_ops, parse_error
        ) VALUES (?1, ?2, 'unavailable', ?3, ?4, NULL, 0, ?5)",
        params![
            event_id,
            tracer,
            started_at,
            Utc::now().to_rfc3339(),
            reason
        ],
    )
    .context("recording unavailable trace")?;
    Ok(conn.last_insert_rowid())
}

pub fn record_captured(
    conn: &Connection,
    project_dir: &Path,
    event_id: i64,
    output_path: &Path,
    started_at: &str,
    keep_raw: bool,
) -> Result<i64> {
    let ended_at = Utc::now().to_rfc3339();
    let parsed = parse_strace_file(output_path, project_dir)?;
    let outside_workspace_ops = parsed
        .iter()
        .filter(|event| !event.within_workspace && (event.path.is_some() || event.path2.is_some()))
        .count() as i64;

    conn.execute(
        "INSERT INTO command_traces (
            event_id, tracer, status, started_at, ended_at, raw_trace_path,
            outside_workspace_ops, parse_error
        ) VALUES (?1, 'strace', 'captured', ?2, ?3, NULL, ?4, NULL)",
        params![event_id, started_at, ended_at, outside_workspace_ops],
    )
    .context("recording captured trace")?;
    let trace_id = conn.last_insert_rowid();

    let raw_trace_path = if keep_raw {
        let traces_dir = project_dir.join(REWIND_DIR).join("traces");
        fs::create_dir_all(&traces_dir)
            .with_context(|| format!("creating {}", traces_dir.display()))?;
        let relative = format!("{REWIND_DIR}/traces/{event_id}-{trace_id}.strace");
        let final_path = project_dir.join(&relative);
        fs::rename(output_path, &final_path).with_context(|| {
            format!(
                "moving raw trace {} to {}",
                output_path.display(),
                final_path.display()
            )
        })?;
        Some(relative)
    } else {
        let _ = fs::remove_file(output_path);
        None
    };
    if let Some(raw_trace_path) = &raw_trace_path {
        conn.execute(
            "UPDATE command_traces SET raw_trace_path = ?1 WHERE id = ?2",
            params![raw_trace_path, trace_id],
        )
        .context("recording raw trace path")?;
    }

    let mut seq = 0_i64;
    for event in parsed {
        if is_process_operation(&event.operation) {
            conn.execute(
                "INSERT INTO trace_processes (
                    trace_id, pid, parent_pid, operation, executable, timestamp, result
                ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)",
                params![
                    trace_id,
                    event.pid,
                    event.operation,
                    event.executable,
                    event.timestamp,
                    event.result
                ],
            )
            .context("recording trace process event")?;
        }

        if event.path.is_some() || event.path2.is_some() {
            seq += 1;
            conn.execute(
                "INSERT INTO trace_file_events (
                    trace_id, seq, timestamp, pid, operation, path, path2,
                    within_workspace, result, errno, access_kind
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    trace_id,
                    seq,
                    event.timestamp,
                    event.pid,
                    event.operation,
                    event.path,
                    event.path2,
                    if event.within_workspace { 1 } else { 0 },
                    event.result,
                    event.errno,
                    event.access_kind,
                ],
            )
            .context("recording trace file event")?;
        }
    }

    Ok(trace_id)
}

pub fn record_parse_error(
    conn: &Connection,
    event_id: i64,
    output_path: &Path,
    started_at: &str,
    error: &anyhow::Error,
) -> Result<i64> {
    let _ = fs::remove_file(output_path);
    conn.execute(
        "INSERT INTO command_traces (
            event_id, tracer, status, started_at, ended_at, raw_trace_path,
            outside_workspace_ops, parse_error
        ) VALUES (?1, 'strace', 'parse_error', ?2, ?3, NULL, 0, ?4)",
        params![
            event_id,
            started_at,
            Utc::now().to_rfc3339(),
            format!("{error:#}")
        ],
    )
    .context("recording parse error trace")?;
    Ok(conn.last_insert_rowid())
}

pub fn get_trace_for_event(conn: &Connection, event_id: i64) -> Result<Option<CommandTrace>> {
    conn.query_row(
        "SELECT id, event_id, tracer, status, started_at, ended_at, raw_trace_path,
                outside_workspace_ops, parse_error
         FROM command_traces
         WHERE event_id = ?1
         ORDER BY id DESC
         LIMIT 1",
        params![event_id],
        trace_from_row,
    )
    .optional()
    .context("querying command trace")
}

pub fn trace_details(project_dir: &Path, event_id: i64) -> Result<Option<TraceDetails>> {
    let conn = history::ensure_initialized(project_dir)?;
    let Some(trace) = get_trace_for_event(&conn, event_id)? else {
        return Ok(None);
    };
    let files = list_file_events(&conn, trace.id)?;
    let processes = list_process_events(&conn, trace.id)?;
    Ok(Some(TraceDetails {
        trace,
        files,
        processes,
    }))
}

pub fn trace_statuses(conn: &Connection) -> Result<BTreeMap<i64, String>> {
    let mut stmt = conn.prepare(
        "SELECT event_id, status
         FROM command_traces
         WHERE id IN (
             SELECT MAX(id) FROM command_traces GROUP BY event_id
         )",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .context("reading trace statuses")
}

pub fn trace_stats(conn: &Connection) -> Result<TraceStats> {
    let mut stats = TraceStats::default();
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM command_traces GROUP BY status")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
    })?;
    for row in rows {
        let (status, count) = row?;
        stats.total += count;
        match status.as_str() {
            "captured" => stats.captured = count,
            "unavailable" => stats.unavailable = count,
            "failed" => stats.failed = count,
            "parse_error" => stats.parse_error = count,
            _ => {}
        }
    }
    stats.file_events = conn.query_row("SELECT COUNT(*) FROM trace_file_events", [], |row| {
        row.get(0)
    })?;
    stats.process_events =
        conn.query_row("SELECT COUNT(*) FROM trace_processes", [], |row| row.get(0))?;
    Ok(stats)
}

pub fn trace_file_touches_for_path(
    project_dir: &Path,
    path: &str,
) -> Result<Vec<(i64, String, String)>> {
    let requested = validate_relative_path(path)?
        .to_string_lossy()
        .replace('\\', "/");
    let conn = history::ensure_initialized(project_dir)?;
    let mut stmt = conn.prepare(
        "SELECT command_traces.event_id, trace_file_events.operation, trace_file_events.path
         FROM trace_file_events
         JOIN command_traces ON command_traces.id = trace_file_events.trace_id
         WHERE trace_file_events.within_workspace = 1
         ORDER BY command_traces.event_id ASC, trace_file_events.seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut touches = Vec::new();
    for row in rows {
        let (event_id, operation, path) = row?;
        let Some(path) = path else {
            continue;
        };
        if path == requested
            || path
                .strip_prefix(&requested)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            touches.push((event_id, operation, path));
        }
    }
    Ok(touches)
}

pub fn parse_strace_file(path: &Path, workspace: &Path) -> Result<Vec<ParsedTraceEvent>> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    raw.lines()
        .map(|line| parse_strace_line(line, workspace))
        .filter_map(|result| result.transpose())
        .collect()
}

pub fn parse_strace_line(line: &str, workspace: &Path) -> Result<Option<ParsedTraceEvent>> {
    let line = line.trim();
    if line.is_empty() || line.contains("<unfinished ...>") || line.starts_with("--- ") {
        return Ok(None);
    }

    let (prefix, rest) = split_prefix(line);
    let pid = prefix.and_then(|value| value.parse::<i64>().ok());
    let (timestamp, syscall_text) = split_timestamp(rest);
    let Some(paren) = syscall_text.find('(') else {
        return Ok(None);
    };
    let operation = &syscall_text[..paren];
    if !is_interesting_operation(operation) {
        return Ok(None);
    }
    let close = syscall_text
        .find(") =")
        .or_else(|| syscall_text.find(") <"))
        .or_else(|| syscall_text.find(")"))
        .context("missing syscall close")?;
    let args = &syscall_text[paren + 1..close];
    let suffix = syscall_text[close + 1..].trim();
    let (result, errno) = parse_result(suffix);
    let string_args = quoted_args(args);
    let access_kind = classify_access_kind(operation, args).to_owned();

    let mut path = None;
    let mut path2 = None;
    let mut within_workspace = true;
    let mut executable = None;

    if operation == "execve" {
        if let Some(arg) = string_args.first() {
            let classified = classify_path(arg, workspace)?;
            within_workspace = classified.within_workspace;
            executable = Some(executable_name(arg));
            path = classified.path;
        }
    } else if let Some(index) = first_path_arg_index(operation, string_args.len()) {
        if let Some(arg) = string_args.get(index) {
            let classified = classify_path(arg, workspace)?;
            within_workspace &= classified.within_workspace;
            path = classified.path;
        }
        if uses_second_path(operation) {
            if let Some(arg) = string_args.get(index + 1) {
                let classified = classify_path(arg, workspace)?;
                within_workspace &= classified.within_workspace;
                path2 = classified.path;
            }
        }
    }

    if path.is_none() && path2.is_none() && !is_process_operation(operation) {
        return Ok(None);
    }

    Ok(Some(ParsedTraceEvent {
        timestamp,
        pid,
        operation: operation.to_owned(),
        path,
        path2,
        within_workspace,
        result,
        errno,
        executable,
        access_kind,
    }))
}

pub fn classify_access_kind(operation: &str, args: &str) -> &'static str {
    match operation {
        "execve" => "execute",
        "open" | "openat" | "openat2" | "creat" => {
            if args.contains("O_CREAT") {
                "create"
            } else if args.contains("O_WRONLY")
                || args.contains("O_RDWR")
                || args.contains("O_TRUNC")
                || args.contains("O_APPEND")
            {
                "write"
            } else if args.contains("O_RDONLY") || operation.starts_with("open") {
                "read"
            } else {
                "unknown"
            }
        }
        "unlink" | "unlinkat" | "rmdir" => "delete",
        "rename" | "renameat" | "renameat2" => "rename",
        "mkdir" | "mkdirat" | "symlink" | "symlinkat" | "link" | "linkat" => "create",
        "stat" | "lstat" | "fstat" | "newfstatat" | "statx" | "access" | "faccessat"
        | "readlink" | "readlinkat" | "chmod" | "fchmod" | "fchmodat" | "chdir" | "fchdir" => {
            "metadata"
        }
        "truncate" | "ftruncate" => "write",
        _ => "unknown",
    }
}

pub fn valid_access_kind(value: &str) -> bool {
    matches!(
        value,
        "read" | "write" | "create" | "delete" | "rename" | "metadata" | "execute" | "unknown"
    )
}

fn split_prefix(line: &str) -> (Option<&str>, &str) {
    let Some((prefix, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    if prefix.chars().all(|ch| ch.is_ascii_digit()) {
        (Some(prefix), rest.trim_start())
    } else {
        (None, line)
    }
}

fn split_timestamp(line: &str) -> (Option<String>, &str) {
    let Some((first, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    if first.contains(':')
        && first
            .chars()
            .all(|ch| ch.is_ascii_digit() || ch == ':' || ch == '.')
    {
        (Some(first.to_owned()), rest.trim_start())
    } else {
        (None, line)
    }
}

fn parse_result(suffix: &str) -> (Option<String>, Option<String>) {
    let suffix = suffix.trim_start_matches('=').trim();
    if suffix.is_empty() {
        return (None, None);
    }
    let result = suffix.split_whitespace().next().map(str::to_owned);
    let errno = if result.as_deref() == Some("-1") {
        suffix.split_whitespace().nth(1).map(str::to_owned)
    } else {
        None
    };
    (result, errno)
}

fn quoted_args(args: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut in_quote = false;
    let mut escaped = false;
    let mut current = String::new();

    for ch in args.chars() {
        if !in_quote {
            if ch == '"' {
                in_quote = true;
                current.clear();
            }
            continue;
        }
        if escaped {
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            values.push(current.clone());
            current.clear();
            in_quote = false;
        } else {
            current.push(ch);
        }
    }
    values
}

struct ClassifiedPath {
    path: Option<String>,
    within_workspace: bool,
}

fn classify_path(raw: &str, workspace: &Path) -> Result<ClassifiedPath> {
    if raw.is_empty() {
        return Ok(ClassifiedPath {
            path: None,
            within_workspace: true,
        });
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(workspace) {
            if let Some(relative) = relative.to_str() {
                if let Ok(valid) = validate_relative_path(relative) {
                    return Ok(ClassifiedPath {
                        path: Some(valid.to_string_lossy().replace('\\', "/")),
                        within_workspace: true,
                    });
                }
            }
        }
        return Ok(ClassifiedPath {
            path: Some(OUTSIDE_WORKSPACE.to_owned()),
            within_workspace: false,
        });
    }

    match validate_relative_path(raw) {
        Ok(valid) => Ok(ClassifiedPath {
            path: Some(valid.to_string_lossy().replace('\\', "/")),
            within_workspace: true,
        }),
        Err(_) => Ok(ClassifiedPath {
            path: Some(OUTSIDE_WORKSPACE.to_owned()),
            within_workspace: false,
        }),
    }
}

fn executable_name(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(raw)
        .to_owned()
}

fn first_path_arg_index(operation: &str, len: usize) -> Option<usize> {
    match operation {
        "openat" | "openat2" | "mkdirat" | "unlinkat" | "renameat" | "renameat2" | "readlinkat"
        | "faccessat" | "fchmodat" | "symlinkat" | "linkat" | "newfstatat" => Some(0),
        "fstat" | "fchdir" | "fchmod" | "ftruncate" => None,
        _ => {
            if len > 0 {
                Some(0)
            } else {
                None
            }
        }
    }
}

fn uses_second_path(operation: &str) -> bool {
    matches!(
        operation,
        "rename" | "renameat" | "renameat2" | "symlink" | "symlinkat" | "link" | "linkat"
    )
}

fn is_interesting_operation(operation: &str) -> bool {
    matches!(
        operation,
        "execve"
            | "clone"
            | "fork"
            | "vfork"
            | "exit"
            | "exit_group"
            | "open"
            | "openat"
            | "openat2"
            | "creat"
            | "stat"
            | "lstat"
            | "fstat"
            | "newfstatat"
            | "statx"
            | "access"
            | "faccessat"
            | "readlink"
            | "readlinkat"
            | "unlink"
            | "unlinkat"
            | "rename"
            | "renameat"
            | "renameat2"
            | "mkdir"
            | "mkdirat"
            | "rmdir"
            | "chdir"
            | "fchdir"
            | "chmod"
            | "fchmod"
            | "fchmodat"
            | "truncate"
            | "ftruncate"
            | "symlink"
            | "symlinkat"
            | "link"
            | "linkat"
    )
}

fn is_process_operation(operation: &str) -> bool {
    matches!(
        operation,
        "execve" | "clone" | "fork" | "vfork" | "exit" | "exit_group"
    )
}

fn strace_supported() -> bool {
    cfg!(target_os = "linux")
        && Command::new("strace")
            .arg("-V")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
}

fn unavailable_reason() -> String {
    if cfg!(target_os = "linux") {
        "strace is not available; install strace or use --trace=off".to_owned()
    } else {
        "strace tracing is only supported on Linux".to_owned()
    }
}

fn trace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CommandTrace> {
    Ok(CommandTrace {
        id: row.get(0)?,
        event_id: row.get(1)?,
        tracer: row.get(2)?,
        status: row.get(3)?,
        started_at: row.get(4)?,
        ended_at: row.get(5)?,
        raw_trace_path: row.get(6)?,
        outside_workspace_ops: row.get(7)?,
        parse_error: row.get(8)?,
    })
}

fn list_file_events(conn: &Connection, trace_id: i64) -> Result<Vec<TraceFileEvent>> {
    let mut stmt = conn.prepare(
        "SELECT seq, timestamp, pid, operation, path, path2, within_workspace, result, errno,
                access_kind
         FROM trace_file_events
         WHERE trace_id = ?1
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![trace_id], |row| {
        let within_workspace: i64 = row.get(6)?;
        Ok(TraceFileEvent {
            seq: row.get(0)?,
            timestamp: row.get(1)?,
            pid: row.get(2)?,
            operation: row.get(3)?,
            path: row.get(4)?,
            path2: row.get(5)?,
            within_workspace: within_workspace != 0,
            result: row.get(7)?,
            errno: row.get(8)?,
            access_kind: row.get(9)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading trace file events")
}

fn list_process_events(conn: &Connection, trace_id: i64) -> Result<Vec<TraceProcessEvent>> {
    let mut stmt = conn.prepare(
        "SELECT pid, parent_pid, operation, executable, timestamp, result
         FROM trace_processes
         WHERE trace_id = ?1
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![trace_id], |row| {
        Ok(TraceProcessEvent {
            pid: row.get(0)?,
            parent_pid: row.get(1)?,
            operation: row.get(2)?,
            executable: row.get(3)?,
            timestamp: row.get(4)?,
            result: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("reading trace process events")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_openat_relative_path() {
        let event = parse_strace_line(
            r#"1234 10:00:00.000000 openat(AT_FDCWD, "notes.txt", O_WRONLY|O_CREAT|O_TRUNC, 0666) = 3"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.operation, "openat");
        assert_eq!(event.pid, Some(1234));
        assert_eq!(event.path.as_deref(), Some("notes.txt"));
        assert!(event.within_workspace);
        assert_eq!(event.result.as_deref(), Some("3"));
        assert_eq!(event.access_kind, "create");
    }

    #[test]
    fn parses_unlink_path() {
        let event = parse_strace_line(r#"1234 unlink("notes.txt") = 0"#, Path::new("/workspace"))
            .expect("parsed")
            .expect("event");

        assert_eq!(event.operation, "unlink");
        assert_eq!(event.path.as_deref(), Some("notes.txt"));
    }

    #[test]
    fn parses_rename_old_and_new_paths() {
        let event = parse_strace_line(
            r#"1234 rename("old.txt", "new.txt") = 0"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.operation, "rename");
        assert_eq!(event.path.as_deref(), Some("old.txt"));
        assert_eq!(event.path2.as_deref(), Some("new.txt"));
    }

    #[test]
    fn parses_mkdir_path() {
        let event = parse_strace_line(r#"1234 mkdir("src", 0777) = 0"#, Path::new("/workspace"))
            .expect("parsed")
            .expect("event");

        assert_eq!(event.operation, "mkdir");
        assert_eq!(event.path.as_deref(), Some("src"));
    }

    #[test]
    fn parses_execve_executable() {
        let event = parse_strace_line(
            r#"1234 execve("/bin/sh", ["sh", "-c", "echo hello"], 0x7ffc) = 0"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.operation, "execve");
        assert_eq!(event.executable.as_deref(), Some("sh"));
    }

    #[test]
    fn redacts_absolute_outside_workspace_paths() {
        let event = parse_strace_line(
            r#"1234 openat(AT_FDCWD, "/etc/ld.so.cache", O_RDONLY|O_CLOEXEC) = 3"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.path.as_deref(), Some(OUTSIDE_WORKSPACE));
        assert!(!event.within_workspace);
    }

    #[test]
    fn classifies_absolute_workspace_paths() {
        let event = parse_strace_line(
            r#"1234 openat(AT_FDCWD, "/workspace/src/main.rs", O_RDONLY) = 3"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.path.as_deref(), Some("src/main.rs"));
        assert!(event.within_workspace);
    }

    #[test]
    fn captures_errno_for_failed_syscall() {
        let event = parse_strace_line(
            r#"1234 unlink("missing.txt") = -1 ENOENT (No such file or directory)"#,
            Path::new("/workspace"),
        )
        .expect("parsed")
        .expect("event");

        assert_eq!(event.operation, "unlink");
        assert_eq!(event.path.as_deref(), Some("missing.txt"));
        assert_eq!(event.result.as_deref(), Some("-1"));
        assert_eq!(event.errno.as_deref(), Some("ENOENT"));
    }

    #[test]
    fn ignores_unsupported_no_path_lines() {
        let event =
            parse_strace_line(r#"1234 getpid() = 1234"#, Path::new("/workspace")).expect("parsed");

        assert!(event.is_none());
    }

    #[test]
    fn classifies_read_operations() {
        assert_eq!(
            classify_access_kind("openat", r#"AT_FDCWD, "notes.txt", O_RDONLY"#),
            "read"
        );
    }

    #[test]
    fn classifies_write_operations() {
        assert_eq!(
            classify_access_kind("openat", r#"AT_FDCWD, "notes.txt", O_WRONLY|O_TRUNC"#),
            "write"
        );
    }

    #[test]
    fn classifies_create_operations() {
        assert_eq!(classify_access_kind("mkdir", r#""src", 0777"#), "create");
    }

    #[test]
    fn classifies_delete_operations() {
        assert_eq!(classify_access_kind("unlink", r#""notes.txt""#), "delete");
    }

    #[test]
    fn classifies_rename_operations() {
        assert_eq!(
            classify_access_kind("rename", r#""old.txt", "new.txt""#),
            "rename"
        );
    }

    #[test]
    fn classifies_metadata_operations() {
        assert_eq!(
            classify_access_kind("stat", r#""notes.txt", 0x0"#),
            "metadata"
        );
    }

    #[test]
    fn classifies_unknown_operations() {
        assert_eq!(classify_access_kind("mystery", ""), "unknown");
    }
}
