use crate::snapshot::{create_snapshot, write_snapshot};
use crate::REWIND_DIR;
use crate::{config, history, repo};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub fn init_project(project_dir: &Path) -> Result<()> {
    let rewind_dir = project_dir.join(REWIND_DIR);
    fs::create_dir_all(rewind_dir.join("objects"))
        .with_context(|| format!("creating {}", rewind_dir.join("objects").display()))?;
    fs::create_dir_all(rewind_dir.join("snapshots"))
        .with_context(|| format!("creating {}", rewind_dir.join("snapshots").display()))?;
    config::write_default_config_if_missing(project_dir)?;
    let conn = history::open(project_dir)?;
    history::initialize_schema(&conn)?;
    let snapshot = create_snapshot(project_dir)?;
    write_snapshot(project_dir, &snapshot)?;
    history::set_head_snapshot(&conn, &snapshot.id)?;
    repo::create_current_repo_metadata(project_dir)?;
    Ok(())
}
