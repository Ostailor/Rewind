use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub struct ObjectStore {
    root: PathBuf,
}

impl ObjectStore {
    pub fn new(rewind_dir: &Path) -> Self {
        Self {
            root: rewind_dir.join("objects"),
        }
    }

    pub fn store_file(&self, path: &Path) -> Result<(String, u64)> {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let hash = sha256_hex(&bytes);
        let object_path = self.object_path(&hash);

        if !object_path.exists() {
            fs::create_dir_all(&self.root)
                .with_context(|| format!("creating {}", self.root.display()))?;
            let tmp_path = self.root.join(format!("{hash}.tmp"));
            fs::write(&tmp_path, &bytes)
                .with_context(|| format!("writing {}", tmp_path.display()))?;
            fs::rename(&tmp_path, &object_path).with_context(|| {
                format!(
                    "renaming {} to {}",
                    tmp_path.display(),
                    object_path.display()
                )
            })?;
        }

        Ok((hash, bytes.len() as u64))
    }

    pub fn object_path(&self, hash: &str) -> PathBuf {
        self.root.join(hash)
    }
}

pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok((sha256_hex(&bytes), bytes.len() as u64))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
