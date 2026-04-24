use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

pub fn validate_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
    if path.is_absolute() {
        bail!("snapshot path must be relative: {}", path.display());
    }

    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                bail!("snapshot path must not contain `..`: {}", path.display())
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("snapshot path must be relative: {}", path.display())
            }
        }
    }

    if components.is_empty() {
        bail!("snapshot path must not be empty");
    }
    if components
        .first()
        .is_some_and(|component| component == REWIND_DIR)
    {
        bail!(
            "snapshot path must not point inside {REWIND_DIR}/: {}",
            path.display()
        );
    }

    Ok(components.iter().collect())
}

pub fn validate_snapshot_paths<'a>(
    directories: impl IntoIterator<Item = &'a String>,
    files: impl IntoIterator<Item = &'a String>,
) -> Result<()> {
    for path in directories {
        validate_relative_path(path)
            .with_context(|| format!("validating directory path {path}"))?;
    }
    for path in files {
        validate_relative_path(path).with_context(|| format!("validating file path {path}"))?;
    }
    Ok(())
}

pub fn ensure_no_symlink_in_path(project_dir: &Path, relative_path: &Path) -> Result<()> {
    let mut current = project_dir.to_path_buf();
    let components = relative_path.components().collect::<Vec<_>>();
    let last_index = components.len().saturating_sub(1);
    for (index, component) in components.into_iter().enumerate() {
        let Component::Normal(value) = component else {
            bail!("unsafe restore path: {}", relative_path.display());
        };
        current.push(value);
        if index == last_index {
            break;
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() {
                bail!(
                    "refusing to modify path through symlink: {}",
                    current.display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_relative_path;

    #[test]
    fn rejects_absolute_paths() {
        assert!(validate_relative_path("/absolute/path").is_err());
    }

    #[test]
    fn rejects_parent_components() {
        assert!(validate_relative_path("../outside.txt").is_err());
        assert!(validate_relative_path("nested/../../outside.txt").is_err());
    }

    #[test]
    fn rejects_rewind_paths() {
        assert!(validate_relative_path(".rewind/events.db").is_err());
        assert!(validate_relative_path(".rewind/objects/foo").is_err());
    }

    #[test]
    fn accepts_normal_relative_paths() {
        assert!(validate_relative_path("src/main.rs").is_ok());
    }
}
