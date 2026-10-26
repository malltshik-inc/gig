use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::model::OllamaClient;
use crate::ui;

pub struct Preflight {
    pub root: PathBuf,
    pub branch: String,
    pub unborn: bool,
    pub git_dir: PathBuf,
}

pub async fn run(dir: &Path, client: &OllamaClient) -> Result<Preflight> {
    git::require_git()?;

    let dir = dir
        .canonicalize()
        .with_context(|| format!("cannot access {}", dir.display()))?;
    let root = git::toplevel(&dir)?;

    let status = git::status_porcelain(&root)?;
    let dirty: Vec<String> = status
        .lines()
        .filter(|line| !line.starts_with("??"))
        .map(|line| line.to_string())
        .collect();
    if !dirty.is_empty() {
        bail!(
            "the working tree has uncommitted changes:\n{}\ncommit or stash them before running gig",
            dirty.join("\n")
        );
    }

    let unborn = !git::head_exists(&root)?;
    let branch = git::branch(&root);
    let git_dir = git::git_dir(&root)?;

    if git::config(&root, "user.name").is_none() || git::config(&root, "user.email").is_none() {
        ui::warn("git user.name / user.email are not configured; the final commit may fail");
    }

    client.ensure_model().await?;

    Ok(Preflight {
        root,
        branch,
        unborn,
        git_dir,
    })
}
