use crate::history;
use crate::object_store::sha256_hex;
use crate::transaction;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const CURRENT_REPO_FORMAT_VERSION: u32 = 2;
pub const CURRENT_DB_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoManifest {
    pub format_version: u32,
    pub db_schema_version: u32,
    pub repo_id: String,
    pub created_at: String,
    pub created_by_version: String,
    pub last_migrated_at: String,
    pub last_migrated_by_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoStatus {
    Uninitialized,
    Current,
    NeedsMigration,
    IncompatibleFutureFormat,
    Invalid,
}

impl RepoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Uninitialized => "uninitialized",
            Self::Current => "current",
            Self::NeedsMigration => "needs migration",
            Self::IncompatibleFutureFormat => "incompatible future format",
            Self::Invalid => "invalid manifest/schema",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RepoOpenResult {
    pub status: RepoStatus,
    pub manifest: Option<RepoManifest>,
    pub db_schema_version: Option<u32>,
    pub reason: Option<String>,
    pub active_journal: bool,
}

#[derive(Debug, Clone)]
pub struct RepoCounts {
    pub events: i64,
    pub checkpoints: i64,
    pub snapshots: usize,
    pub objects: usize,
    pub head_snapshot: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub rewind_dir: PathBuf,
    pub status: RepoOpenResult,
    pub counts: Option<RepoCounts>,
}

#[derive(Debug, Clone)]
pub struct MigrationSummary {
    pub changed: bool,
    pub old_status: RepoStatus,
    pub new_status: RepoStatus,
    pub steps: Vec<String>,
}

pub fn manifest_path(project_dir: &Path) -> PathBuf {
    project_dir.join(REWIND_DIR).join("repo.json")
}

pub fn inspect(project_dir: &Path) -> RepoOpenResult {
    inspect_inner(project_dir).unwrap_or_else(|error| RepoOpenResult {
        status: RepoStatus::Invalid,
        manifest: None,
        db_schema_version: None,
        reason: Some(format!("{error:#}")),
        active_journal: transaction::has_active(project_dir),
    })
}

fn inspect_inner(project_dir: &Path) -> Result<RepoOpenResult> {
    let rewind_dir = project_dir.join(REWIND_DIR);
    let active_journal = transaction::has_active(project_dir);
    if !rewind_dir.is_dir() {
        return Ok(RepoOpenResult {
            status: RepoStatus::Uninitialized,
            manifest: None,
            db_schema_version: None,
            reason: Some("not initialized; run `rewind init` first".to_owned()),
            active_journal,
        });
    }

    let manifest = read_manifest(project_dir);
    let db_schema_version = read_db_schema_version(project_dir)?;

    let manifest = match manifest {
        Ok(Some(manifest)) => Some(manifest),
        Ok(None) => {
            return Ok(RepoOpenResult {
                status: RepoStatus::NeedsMigration,
                manifest: None,
                db_schema_version,
                reason: Some(".rewind/repo.json is missing".to_owned()),
                active_journal,
            });
        }
        Err(error) => {
            return Ok(RepoOpenResult {
                status: RepoStatus::Invalid,
                manifest: None,
                db_schema_version,
                reason: Some(format!("invalid .rewind/repo.json: {error:#}")),
                active_journal,
            });
        }
    };

    let Some(manifest) = manifest else {
        unreachable!("manifest missing handled above");
    };

    if let Err(error) = validate_manifest_shape(&manifest) {
        return Ok(RepoOpenResult {
            status: RepoStatus::Invalid,
            manifest: Some(manifest),
            db_schema_version,
            reason: Some(format!("{error:#}")),
            active_journal,
        });
    }

    if manifest.format_version > CURRENT_REPO_FORMAT_VERSION
        || manifest.db_schema_version > CURRENT_DB_SCHEMA_VERSION
        || db_schema_version.is_some_and(|version| version > CURRENT_DB_SCHEMA_VERSION)
    {
        return Ok(RepoOpenResult {
            status: RepoStatus::IncompatibleFutureFormat,
            manifest: Some(manifest),
            db_schema_version,
            reason: Some(format!(
                "supported format/schema: {}/{}",
                CURRENT_REPO_FORMAT_VERSION, CURRENT_DB_SCHEMA_VERSION
            )),
            active_journal,
        });
    }

    let Some(db_schema_version) = db_schema_version else {
        return Ok(RepoOpenResult {
            status: RepoStatus::NeedsMigration,
            manifest: Some(manifest),
            db_schema_version: None,
            reason: Some("schema metadata is missing".to_owned()),
            active_journal,
        });
    };

    if manifest.format_version != CURRENT_REPO_FORMAT_VERSION
        || manifest.db_schema_version != CURRENT_DB_SCHEMA_VERSION
    {
        return Ok(RepoOpenResult {
            status: RepoStatus::NeedsMigration,
            manifest: Some(manifest),
            db_schema_version: Some(db_schema_version),
            reason: Some("repo format or schema is older than this Rewind version".to_owned()),
            active_journal,
        });
    }

    if manifest.db_schema_version != db_schema_version {
        let reason = format!(
            "manifest db_schema_version {} does not match SQLite schema version {}",
            manifest.db_schema_version, db_schema_version
        );
        return Ok(RepoOpenResult {
            status: RepoStatus::Invalid,
            manifest: Some(manifest),
            db_schema_version: Some(db_schema_version),
            reason: Some(reason),
            active_journal,
        });
    }

    Ok(RepoOpenResult {
        status: RepoStatus::Current,
        manifest: Some(manifest),
        db_schema_version: Some(db_schema_version),
        reason: None,
        active_journal,
    })
}

pub fn ensure_current(project_dir: &Path) -> Result<()> {
    let status = inspect(project_dir);
    match status.status {
        RepoStatus::Current => Ok(()),
        RepoStatus::Uninitialized => bail!(
            "{} is not initialized; run `rewind init` first",
            project_dir.display()
        ),
        RepoStatus::NeedsMigration => bail!(
            "This Rewind repo needs migration. Run: rewind migrate{}",
            reason_suffix(&status)
        ),
        RepoStatus::IncompatibleFutureFormat => bail!(
            "This Rewind repo uses a newer unsupported format.{}",
            reason_suffix(&status)
        ),
        RepoStatus::Invalid => bail!(
            "This Rewind repo has invalid format metadata. Run: rewind doctor{}",
            reason_suffix(&status)
        ),
    }
}

pub fn repo_info(project_dir: &Path) -> RepoInfo {
    let status = inspect(project_dir);
    let counts = if project_dir.join(REWIND_DIR).is_dir() {
        Some(read_counts(project_dir).unwrap_or_else(|_| RepoCounts {
            events: 0,
            checkpoints: 0,
            snapshots: count_entries(project_dir.join(REWIND_DIR).join("snapshots")),
            objects: count_entries(project_dir.join(REWIND_DIR).join("objects")),
            head_snapshot: None,
        }))
    } else {
        None
    };

    RepoInfo {
        rewind_dir: project_dir.join(REWIND_DIR),
        status,
        counts,
    }
}

pub fn migrate(project_dir: &Path) -> Result<MigrationSummary> {
    let old = inspect(project_dir);
    if old.active_journal {
        bail!("cannot migrate while an active Rewind transaction exists; run `rewind recover --status`");
    }
    match old.status {
        RepoStatus::Uninitialized => bail!(
            "{} is not initialized; run `rewind init` first",
            project_dir.display()
        ),
        RepoStatus::IncompatibleFutureFormat => {
            bail!("cannot migrate a repo from a newer unsupported format")
        }
        RepoStatus::Invalid => bail!(
            "cannot migrate invalid repo metadata automatically; run `rewind doctor` for details"
        ),
        RepoStatus::Current => {
            return Ok(MigrationSummary {
                changed: false,
                old_status: RepoStatus::Current,
                new_status: RepoStatus::Current,
                steps: Vec::new(),
            });
        }
        RepoStatus::NeedsMigration => {}
    }

    let rewind_dir = project_dir.join(REWIND_DIR);
    fs::create_dir_all(rewind_dir.join("objects"))
        .with_context(|| format!("creating {}", rewind_dir.join("objects").display()))?;
    fs::create_dir_all(rewind_dir.join("snapshots"))
        .with_context(|| format!("creating {}", rewind_dir.join("snapshots").display()))?;
    let conn = history::open(project_dir)?;
    history::initialize_schema(&conn)?;
    set_db_schema_version(&conn, CURRENT_DB_SCHEMA_VERSION)?;

    let now = Utc::now().to_rfc3339();
    let repo_id = old
        .manifest
        .as_ref()
        .map(|manifest| manifest.repo_id.clone())
        .filter(|repo_id| !repo_id.is_empty())
        .unwrap_or_else(|| generate_repo_id(project_dir, &now));
    let created_at = old
        .manifest
        .as_ref()
        .map(|manifest| manifest.created_at.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| now.clone());
    let created_by_version = old
        .manifest
        .as_ref()
        .map(|manifest| manifest.created_by_version.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(app_version);
    let manifest = RepoManifest {
        format_version: CURRENT_REPO_FORMAT_VERSION,
        db_schema_version: CURRENT_DB_SCHEMA_VERSION,
        repo_id,
        created_at,
        created_by_version,
        last_migrated_at: now,
        last_migrated_by_version: app_version(),
    };
    write_manifest(project_dir, &manifest)?;

    let new_status = inspect(project_dir);
    let mut steps = Vec::new();
    if old.manifest.is_none() {
        steps.push("created repo manifest".to_owned());
    } else {
        steps.push("updated repo manifest".to_owned());
    }
    if old.db_schema_version.is_none() {
        steps.push("initialized DB schema metadata".to_owned());
    } else {
        steps.push("updated DB schema metadata".to_owned());
    }
    steps.push(format!(
        "set format_version = {}",
        CURRENT_REPO_FORMAT_VERSION
    ));
    steps.push(format!(
        "set db_schema_version = {}",
        CURRENT_DB_SCHEMA_VERSION
    ));

    Ok(MigrationSummary {
        changed: true,
        old_status: old.status,
        new_status: new_status.status,
        steps,
    })
}

pub fn create_current_repo_metadata(project_dir: &Path) -> Result<()> {
    let conn = history::open(project_dir)?;
    set_db_schema_version(&conn, CURRENT_DB_SCHEMA_VERSION)?;
    if read_manifest(project_dir)?.is_none() {
        let now = Utc::now().to_rfc3339();
        let manifest = RepoManifest {
            format_version: CURRENT_REPO_FORMAT_VERSION,
            db_schema_version: CURRENT_DB_SCHEMA_VERSION,
            repo_id: generate_repo_id(project_dir, &now),
            created_at: now.clone(),
            created_by_version: app_version(),
            last_migrated_at: now,
            last_migrated_by_version: app_version(),
        };
        write_manifest(project_dir, &manifest)?;
    }
    Ok(())
}

pub fn read_manifest(project_dir: &Path) -> Result<Option<RepoManifest>> {
    let path = manifest_path(project_dir);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice::<RepoManifest>(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
        .map(Some)
}

pub fn write_manifest(project_dir: &Path, manifest: &RepoManifest) -> Result<()> {
    let path = manifest_path(project_dir);
    let tmp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest)?;
    fs::write(&tmp_path, bytes).with_context(|| format!("writing {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("renaming {} to {}", tmp_path.display(), path.display()))?;
    Ok(())
}

pub fn read_db_schema_version(project_dir: &Path) -> Result<Option<u32>> {
    let db_path = project_dir.join(REWIND_DIR).join("events.db");
    if !db_path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    if !table_exists(&conn, "schema_meta")? {
        return Ok(None);
    }
    let value = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'db_schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .context("reading schema_meta db_schema_version")?;
    value
        .map(|value| {
            value
                .parse::<u32>()
                .with_context(|| format!("invalid db_schema_version {value}"))
        })
        .transpose()
}

pub fn set_db_schema_version(conn: &Connection, version: u32) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )
    .context("creating schema_meta table")?;
    conn.execute(
        "INSERT INTO schema_meta (key, value)
         VALUES ('db_schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![version.to_string()],
    )
    .context("setting db schema version")?;
    Ok(())
}

pub fn validate_manifest_shape(manifest: &RepoManifest) -> Result<()> {
    if manifest.format_version == 0 {
        bail!("format_version must be greater than zero");
    }
    if manifest.db_schema_version == 0 {
        bail!("db_schema_version must be greater than zero");
    }
    if manifest.repo_id.trim().is_empty() {
        bail!("repo_id must be present");
    }
    if manifest.created_at.trim().is_empty() {
        bail!("created_at must be present");
    }
    if manifest.created_by_version.trim().is_empty() {
        bail!("created_by_version must be present");
    }
    if manifest.last_migrated_at.trim().is_empty() {
        bail!("last_migrated_at must be present");
    }
    if manifest.last_migrated_by_version.trim().is_empty() {
        bail!("last_migrated_by_version must be present");
    }
    Ok(())
}

fn read_counts(project_dir: &Path) -> Result<RepoCounts> {
    let db_path = project_dir.join(REWIND_DIR).join("events.db");
    if !db_path.exists() {
        return Ok(RepoCounts {
            events: 0,
            checkpoints: 0,
            snapshots: count_entries(project_dir.join(REWIND_DIR).join("snapshots")),
            objects: count_entries(project_dir.join(REWIND_DIR).join("objects")),
            head_snapshot: None,
        });
    }
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", db_path.display()))?;
    let events = count_table(&conn, "events")?;
    let checkpoints = count_table(&conn, "checkpoints")?;
    let head_snapshot = if table_exists(&conn, "workspace_state")? {
        conn.query_row(
            "SELECT value FROM workspace_state WHERE key = 'head_snapshot'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    } else {
        None
    };

    Ok(RepoCounts {
        events,
        checkpoints,
        snapshots: count_entries(project_dir.join(REWIND_DIR).join("snapshots")),
        objects: count_entries(project_dir.join(REWIND_DIR).join("objects")),
        head_snapshot,
    })
}

fn count_table(conn: &Connection, table: &str) -> Result<i64> {
    if !table_exists(conn, table)? {
        return Ok(0);
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    conn.query_row(&sql, [], |row| row.get(0))
        .with_context(|| format!("counting {table}"))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn count_entries(path: PathBuf) -> usize {
    fs::read_dir(path)
        .map(|entries| entries.count())
        .unwrap_or(0)
}

fn generate_repo_id(project_dir: &Path, created_at: &str) -> String {
    let seed = format!(
        "{}|{}|{}",
        created_at,
        project_dir.display(),
        std::process::id()
    );
    sha256_hex(seed.as_bytes())
}

fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn reason_suffix(status: &RepoOpenResult) -> String {
    status
        .reason
        .as_ref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default()
}
