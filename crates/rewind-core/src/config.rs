use crate::ignore::IgnoreRules;
use crate::path_safety::validate_relative_path;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_PATH: &str = ".rewind/config.toml";
pub const DEFAULT_IGNORE_FILE: &str = ".rewindignore";

#[derive(Debug, Clone)]
pub struct RewindConfig {
    pub ignore: IgnoreConfig,
}

#[derive(Debug, Clone)]
pub struct IgnoreConfig {
    pub enabled: bool,
    pub file: String,
}

#[derive(Debug, Clone)]
pub struct ConfigStatus {
    pub config: RewindConfig,
    pub config_exists: bool,
    pub config_source: String,
    pub ignore_file_exists: bool,
    pub ignore_rules_loaded: bool,
    pub ignore_rule_count: usize,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            ignore: IgnoreConfig {
                enabled: true,
                file: DEFAULT_IGNORE_FILE.to_owned(),
            },
        }
    }
}

pub fn config_path(project_dir: &Path) -> PathBuf {
    project_dir.join(CONFIG_PATH)
}

pub fn load_config(project_dir: &Path) -> Result<(RewindConfig, bool)> {
    let path = config_path(project_dir);
    if !path.exists() {
        return Ok((RewindConfig::default(), false));
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config = parse_config(&text)
        .with_context(|| format!("Invalid Rewind config at {}:", CONFIG_PATH))?;
    validate_ignore_file_path(&config.ignore.file)?;
    Ok((config, true))
}

pub fn load_ignore_rules(project_dir: &Path) -> Result<Option<IgnoreRules>> {
    let (config, _) = load_config(project_dir)?;
    if !config.ignore.enabled {
        return Ok(None);
    }
    let ignore_path = project_dir.join(&config.ignore.file);
    if !ignore_path.exists() {
        return Ok(Some(IgnoreRules::empty(config.ignore.file)));
    }
    IgnoreRules::load(project_dir, &config.ignore.file).map(Some)
}

pub fn status(project_dir: &Path) -> Result<ConfigStatus> {
    let (config, config_exists) = load_config(project_dir)?;
    let ignore_file_exists = project_dir.join(&config.ignore.file).exists();
    let rules = load_ignore_rules(project_dir)?;
    Ok(ConfigStatus {
        config,
        config_exists,
        config_source: if config_exists { "file" } else { "defaults" }.to_owned(),
        ignore_file_exists,
        ignore_rules_loaded: rules.is_some(),
        ignore_rule_count: rules.as_ref().map(|rules| rules.len()).unwrap_or(0),
    })
}

pub fn write_default_config_if_missing(project_dir: &Path) -> Result<()> {
    let path = config_path(project_dir);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(
        &path,
        "[ignore]\nenabled = true\nfile = \".rewindignore\"\n",
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn validate_ignore_file_path(path: &str) -> Result<()> {
    validate_relative_path(path)?;
    if path == REWIND_DIR || path.starts_with(".rewind/") {
        bail!("ignore.file must not point inside .rewind/");
    }
    Ok(())
}

fn parse_config(text: &str) -> Result<RewindConfig> {
    let mut config = RewindConfig::default();
    let mut section: Option<String> = None;
    let mut saw_ignore = false;

    for (index, raw_line) in text.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line.trim_start_matches('[').trim_end_matches(']');
            if name != "ignore" {
                bail!("unsupported config section [{name}] on line {line_number}");
            }
            section = Some(name.to_owned());
            saw_ignore = true;
            continue;
        }

        let Some(current_section) = &section else {
            bail!("config key outside a supported section on line {line_number}");
        };
        if current_section != "ignore" {
            bail!("unsupported config section [{current_section}] on line {line_number}");
        }
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("expected key = value on line {line_number}"))?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "enabled" => {
                config.ignore.enabled = match value {
                    "true" => true,
                    "false" => false,
                    _ => bail!("ignore.enabled must be true or false on line {line_number}"),
                };
            }
            "file" => {
                config.ignore.file = parse_string(value).with_context(|| {
                    format!("ignore.file must be a string on line {line_number}")
                })?;
            }
            _ => bail!("unsupported config key ignore.{key} on line {line_number}"),
        }
    }

    if !saw_ignore && !text.trim().is_empty() {
        bail!("config must contain only an [ignore] section");
    }
    validate_ignore_file_path(&config.ignore.file)?;
    Ok(config)
}

fn parse_string(value: &str) -> Result<String> {
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        bail!("expected quoted string");
    }
    Ok(value[1..value.len() - 1].to_owned())
}
