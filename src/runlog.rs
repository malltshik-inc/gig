use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

pub struct RunLog {
    dir: PathBuf,
}

impl RunLog {
    pub fn create(git_dir: &Path) -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let dir = git_dir
            .join("gig")
            .join("runs")
            .join(format!("{stamp}-{}", std::process::id()));
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create run log at {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn write(&self, name: &str, content: &str) {
        let path = self.dir.join(name);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, content);
    }

    pub fn write_json<T: Serialize>(&self, name: &str, value: &T) {
        if let Ok(text) = serde_json::to_string_pretty(value) {
            self.write(name, &text);
        }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}
