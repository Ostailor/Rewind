use crate::config;
use crate::path_safety::validate_relative_path;
use crate::REWIND_DIR;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct IgnoreRules {
    ignore_file: String,
    patterns: Vec<IgnorePattern>,
}

#[derive(Debug, Clone)]
struct IgnorePattern {
    raw: String,
    kind: PatternKind,
    has_slash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternKind {
    Exact,
    Directory,
    Glob,
}

impl IgnoreRules {
    pub fn empty(ignore_file: String) -> Self {
        Self {
            ignore_file,
            patterns: Vec::new(),
        }
    }

    pub fn load(project_dir: &Path, ignore_file: &str) -> Result<Self> {
        config::validate_ignore_file_path(ignore_file)?;
        let path = project_dir.join(ignore_file);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(ignore_file, &text)
            .with_context(|| format!("Invalid ignore rules in {ignore_file}:"))
    }

    pub fn parse(ignore_file: &str, text: &str) -> Result<Self> {
        let mut patterns = Vec::new();
        for (index, raw_line) in text.lines().enumerate() {
            let line_number = index + 1;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            patterns.push(parse_pattern(line).with_context(|| {
                format!("invalid ignore pattern on line {line_number}: {line}")
            })?);
        }
        Ok(Self {
            ignore_file: ignore_file.to_owned(),
            patterns,
        })
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn is_ignored(&self, relative_path: &str, is_dir: bool) -> bool {
        if relative_path == self.ignore_file {
            return false;
        }
        if relative_path == REWIND_DIR || relative_path.starts_with(".rewind/") {
            return true;
        }
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(relative_path, is_dir))
    }
}

impl IgnorePattern {
    fn matches(&self, relative_path: &str, is_dir: bool) -> bool {
        match self.kind {
            PatternKind::Exact => {
                if self.has_slash {
                    relative_path == self.raw
                } else {
                    basename(relative_path) == self.raw
                }
            }
            PatternKind::Directory => {
                directory_matches(&self.raw, self.has_slash, relative_path, is_dir)
            }
            PatternKind::Glob => {
                if self.has_slash {
                    glob_matches(&self.raw, relative_path)
                } else {
                    glob_matches(&self.raw, basename(relative_path))
                }
            }
        }
    }
}

fn parse_pattern(line: &str) -> Result<IgnorePattern> {
    if line.starts_with('!') {
        bail!("negation patterns are not supported");
    }
    if line.starts_with('/') {
        bail!("absolute-style patterns are not supported");
    }
    if line.contains('\\') {
        bail!("backslashes are not supported in ignore patterns");
    }
    let is_directory = line.ends_with('/');
    let body = line.trim_end_matches('/');
    if body.is_empty() {
        bail!("empty ignore pattern");
    }
    if !(body == REWIND_DIR || body.starts_with(".rewind/")) {
        validate_relative_path(body)?;
    }
    let has_glob = body.contains('*') || body.contains('?');
    let has_slash = body.contains('/');
    let kind = if is_directory {
        PatternKind::Directory
    } else if has_glob {
        PatternKind::Glob
    } else {
        PatternKind::Exact
    };
    Ok(IgnorePattern {
        raw: body.to_owned(),
        kind,
        has_slash,
    })
}

fn directory_matches(pattern: &str, has_slash: bool, relative_path: &str, is_dir: bool) -> bool {
    if has_slash {
        relative_path == pattern || relative_path.starts_with(&format!("{pattern}/"))
    } else {
        relative_path
            .split('/')
            .any(|component| component == pattern)
            || (is_dir && basename(relative_path) == pattern)
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    glob_match_bytes(pattern.as_bytes(), text.as_bytes())
}

fn glob_match_bytes(pattern: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star = None;
    let mut match_after_star = 0;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            match_after_star = t;
            p += 1;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            match_after_star += 1;
            t = match_after_star;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
