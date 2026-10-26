use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Remembers the original bytes of every file the agent touches, so a failed
/// attempt can be rolled back without relying on git (works on an unborn HEAD).
pub struct Snapshot {
    root: PathBuf,
    entries: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl Snapshot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            entries: BTreeMap::new(),
        }
    }

    /// Record a file once, before its first write. `None` means the agent
    /// created it and rollback should delete it.
    pub fn record(&mut self, relative: &Path) -> Result<()> {
        if self.entries.contains_key(relative) {
            return Ok(());
        }
        let absolute = self.root.join(relative);
        let original = if absolute.exists() {
            Some(
                fs::read(&absolute)
                    .with_context(|| format!("failed to snapshot {}", absolute.display()))?,
            )
        } else {
            None
        };
        self.entries.insert(relative.to_path_buf(), original);
        Ok(())
    }

    pub fn restore_all(&self) -> Result<Vec<PathBuf>> {
        let mut restored = Vec::new();
        for (relative, original) in self.entries.iter().rev() {
            let absolute = self.root.join(relative);
            match original {
                Some(bytes) => {
                    if let Some(parent) = absolute.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&absolute, bytes)
                        .with_context(|| format!("failed to restore {}", absolute.display()))?;
                }
                None => {
                    if absolute.exists() {
                        fs::remove_file(&absolute)
                            .with_context(|| format!("failed to remove {}", absolute.display()))?;
                    }
                }
            }
            restored.push(relative.clone());
        }
        Ok(restored)
    }

    pub fn touched(&self) -> Vec<PathBuf> {
        self.entries.keys().cloned().collect()
    }

    pub fn original(&self, relative: &Path) -> Option<&[u8]> {
        self.entries
            .get(relative)
            .and_then(|entry| entry.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_modified_and_removes_created_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "old").unwrap();

        let mut snapshot = Snapshot::new(root);
        snapshot.record(Path::new("src/lib.rs")).unwrap();
        snapshot.record(Path::new("src/new.rs")).unwrap();

        fs::write(root.join("src/lib.rs"), "new").unwrap();
        fs::write(root.join("src/new.rs"), "created").unwrap();

        snapshot.restore_all().unwrap();

        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), "old");
        assert!(!root.join("src/new.rs").exists());
    }
}
