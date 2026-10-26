use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

pub fn require_git() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .context("git is not installed or not on PATH")?;
    if !output.status.success() {
        bail!("git is not available");
    }
    Ok(())
}

fn run(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_string()
}

pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let output = run(dir, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        bail!(
            "not a git repository: {}\n{}",
            dir.display(),
            stderr(&output)
        );
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(PathBuf::from(path))
}

pub fn git_dir(root: &Path) -> Result<PathBuf> {
    let output = run(root, &["rev-parse", "--absolute-git-dir"])?;
    if !output.status.success() {
        bail!("cannot locate .git directory: {}", stderr(&output));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

pub fn status_porcelain(root: &Path) -> Result<String> {
    let output = run(root, &["status", "--porcelain"])?;
    if !output.status.success() {
        bail!("git status failed: {}", stderr(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn head_exists(root: &Path) -> Result<bool> {
    let output = run(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    Ok(output.status.success())
}

pub fn branch(root: &Path) -> String {
    run(root, &["symbolic-ref", "--short", "HEAD"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "detached HEAD".to_string())
}

pub fn config(root: &Path, key: &str) -> Option<String> {
    run(root, &["config", "--get", key])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn is_tracked(root: &Path, relative: &str) -> bool {
    run(root, &["ls-files", "--error-unmatch", "--", relative])
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn diff_tracked(root: &Path, relatives: &[String]) -> Result<String> {
    if relatives.is_empty() {
        return Ok(String::new());
    }
    let mut args = vec!["diff", "--no-color", "--"];
    args.extend(relatives.iter().map(String::as_str));
    let output = run(root, &args)?;
    if !output.status.success() {
        bail!("git diff failed: {}", stderr(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn diff_stat(root: &Path, relatives: &[String]) -> Result<String> {
    if relatives.is_empty() {
        return Ok(String::new());
    }
    let mut args = vec!["diff", "--stat", "--no-color", "--"];
    args.extend(relatives.iter().map(String::as_str));
    let output = run(root, &args)?;
    if !output.status.success() {
        bail!("git diff --stat failed: {}", stderr(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn stage(root: &Path, relatives: &[String]) -> Result<()> {
    if relatives.is_empty() {
        return Ok(());
    }
    let mut args = vec!["add", "--"];
    args.extend(relatives.iter().map(String::as_str));
    let output = run(root, &args)?;
    if !output.status.success() {
        bail!("git add failed: {}", stderr(&output));
    }
    Ok(())
}

pub fn commit(root: &Path, message: &str) -> Result<()> {
    let output = run(root, &["commit", "-m", message])?;
    if !output.status.success() {
        bail!(
            "git commit failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
