use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Project {
    pub language: String,
    pub manifest: PathBuf,
    pub name: String,
    pub edition: String,
}

pub fn detect(root: &Path) -> Result<Project> {
    let manifest = root.join("Cargo.toml");
    if !manifest.exists() {
        bail!(
            "no supported project detected at {}\nthis build supports Rust/Cargo only",
            root.display()
        );
    }
    let text = fs_read(&manifest)?;
    let name = find_package_value(&text, "name").unwrap_or_else(|| "unknown".to_string());
    let edition = find_package_value(&text, "edition").unwrap_or_else(|| "2021".to_string());
    Ok(Project {
        language: "rust".to_string(),
        manifest,
        name,
        edition,
    })
}

fn fs_read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

fn find_package_value(text: &str, key: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some((left, right)) = trimmed.split_once('=')
            && left.trim() == key
        {
            let value = right.trim().trim_matches('"').trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_package_name_and_edition() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nedition = \"2024\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        let project = detect(dir.path()).unwrap();
        assert_eq!(project.name, "demo");
        assert_eq!(project.edition, "2024");
    }
}
