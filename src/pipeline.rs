use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::Cli;
use crate::agent;
use crate::detect;
use crate::git;
use crate::lang;
use crate::model::OllamaClient;
use crate::model::Rename;
use crate::preflight;
use crate::repomap;
use crate::runlog::RunLog;
use crate::snapshot::Snapshot;
use crate::ui;
use crate::verify::{self, VerifyCommand};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Plan,
    Build,
}

/// Plan mode is the default. `--build` selects build mode, and `--dry-run`
/// implies build mode. `--plan` always wins; combining it with `--build` is
/// reported as a conflict.
pub fn resolve_mode(plan: bool, build: bool, dry_run: bool) -> (RunMode, bool) {
    let conflict = plan && build;
    let plan_mode = plan || (!build && !dry_run);
    let mode = if plan_mode {
        RunMode::Plan
    } else {
        RunMode::Build
    };
    (mode, conflict)
}

pub async fn run(cli: Cli, prompt: String) -> Result<i32> {
    ui::banner();

    let (mode, conflict) = resolve_mode(cli.plan, cli.build, cli.dry_run);
    if conflict {
        ui::warn("both --plan and --build were given; planning only");
    }
    if cli.plan && cli.dry_run {
        ui::warn("--dry-run has no effect in plan mode");
    }
    let plan_mode = mode == RunMode::Plan;

    let client = OllamaClient::new(cli.model.clone(), cli.num_ctx, cli.verbose);

    ui::stage("preflight");
    let pf = preflight::run(&cli.dir, &client).await?;
    ui::info(&format!("repository: {}", pf.root.display()));
    ui::info(&format!(
        "branch: {}{}",
        pf.branch,
        if pf.unborn { " (no commits yet)" } else { "" }
    ));
    ui::ok(&format!("model available: {}", cli.model));
    if pf.branch == "detached HEAD" {
        ui::warn("HEAD is detached; the commit will not advance a branch");
    }

    let log = RunLog::create(&pf.git_dir)?;
    log.write("task.txt", &prompt);
    ui::detail(&format!("run log: {}", log.path().display()));

    ui::stage("detect");
    let project = detect::detect(&pf.root)?;
    ui::ok(&format!(
        "{} project `{}` (edition {})",
        project.language, project.name, project.edition
    ));
    ui::detail(&format!("manifest: {}", project.manifest.display()));

    let check_command = VerifyCommand {
        label: "cargo check".to_string(),
        program: "cargo".to_string(),
        args: vec!["check".to_string(), "--message-format=json".to_string()],
    };
    let test_command = VerifyCommand {
        label: "cargo test".to_string(),
        program: "cargo".to_string(),
        args: vec!["test".to_string(), "--quiet".to_string()],
    };

    let baseline = if cli.no_baseline {
        ui::warn("baseline verification skipped");
        "(skipped)".to_string()
    } else {
        ui::stage("baseline");
        ui::info("running cargo check and cargo test on the clean tree...");
        let result = verify::run_all(
            &pf.root,
            &[check_command.clone(), test_command.clone()],
            Duration::from_secs(600),
        )
        .await;
        log.write("baseline.txt", &result.summary);
        if result.success {
            ui::ok("baseline is green");
            "clean".to_string()
        } else {
            ui::warn("baseline is already failing; this run requires a fully green build");
            ui::detail(&result.summary);
            result.summary.clone()
        }
    };

    ui::stage("repo map");
    let map = repomap::build(&pf.root, &prompt, 400)?;
    ui::ok(&format!(
        "{} Rust files, {} symbols{}",
        map.total_files,
        map.total_symbols,
        if map.truncated {
            " (truncated for the planner)"
        } else {
            ""
        }
    ));
    let rendered_map = map.render(12000);
    log.write("repo-map.txt", &rendered_map);

    ui::stage("plan");
    let mut task = prompt.clone();
    let mut plan = agent::plan(
        &client,
        &agent::PlanContext {
            task: &task,
            project_name: &project.name,
            edition: &project.edition,
            repo_map: &rendered_map,
            baseline: &baseline,
            max_files: cli.max_files.max(1),
        },
    )
    .await?;
    log.write_json("plan.json", &plan);

    if plan.needs_clarification {
        ui::warn(&format!(
            "the planner needs clarification: {}",
            plan.clarification
        ));
        if cli.yes || plan_mode {
            ui::info("re-run with a task that answers this question");
            return Ok(2);
        }
        let answer = ui::ask("your answer")?;
        if answer.is_empty() {
            ui::info("no answer given; aborted");
            return Ok(2);
        }
        task = format!("{prompt}\n\nClarification: {answer}");
        plan = agent::plan(
            &client,
            &agent::PlanContext {
                task: &task,
                project_name: &project.name,
                edition: &project.edition,
                repo_map: &rendered_map,
                baseline: &baseline,
                max_files: cli.max_files.max(1),
            },
        )
        .await?;
        log.write_json("plan-2.json", &plan);
        if plan.needs_clarification {
            bail!(
                "the planner still needs clarification: {}",
                plan.clarification
            );
        }
    }

    plan = agent::sanitize_plan(plan, &pf.root, cli.max_files.max(1))?;
    if plan.files.is_empty() {
        bail!("the planner produced no actionable edits");
    }

    ui::stage("proposed plan");
    ui::print_plan(&plan);
    if plan_mode {
        ui::info("plan mode; no files were changed");
        return Ok(0);
    }
    if !ui::confirm("proceed with this plan?", true, cli.yes)? {
        ui::info("aborted; no files were changed");
        return Ok(2);
    }

    let max_attempts = cli.max_attempts.max(1);
    let mut snapshot = Snapshot::new(&pf.root);
    let mut previous: BTreeMap<String, String> = BTreeMap::new();
    let mut format_clean: BTreeMap<String, bool> = BTreeMap::new();
    let mut diagnostics = String::new();
    let mut success = false;

    for attempt in 1..=max_attempts {
        ui::stage(&format!("edit attempt {attempt}/{max_attempts}"));
        let mut failure: Option<String> = None;

        if let Some(summary) = apply_renames(&pf.root, &plan.renames, &mut snapshot)? {
            ui::ok(&summary);
        }

        'files: for file in &plan.files {
            let relative = Path::new(&file.path);

            if file.create {
                let edit = agent::edit_new_file(
                    &client,
                    &agent::NewFileContext {
                        task: &task,
                        plan: &plan.plan,
                        file: &file.path,
                        context: &style_reference(&pf.root),
                        diagnostics: &diagnostics,
                        attempt,
                        max_attempts,
                    },
                )
                .await?;
                log.write_json(
                    &format!("attempt-{attempt}/create-{}.json", slug(&file.path)),
                    &edit,
                );
                if edit.give_up {
                    failure = Some(format!(
                        "the editor gave up on {}: {}",
                        file.path, edit.thought
                    ));
                    break 'files;
                }
                if edit.truncated {
                    failure = Some(format!(
                        "the reply for the new file {} was cut off (the model hit the output limit); the file must be complete",
                        file.path
                    ));
                    break 'files;
                }
                let content = apply_plan_renames(&edit.new_code, &plan.renames)?;
                if let Some(problem) = lang::syntax_problem(&content) {
                    failure = Some(format!(
                        "the new file {} does not parse: {problem}",
                        file.path
                    ));
                    break 'files;
                }
                let errors = lang::error_count(&content)?;
                if errors > 0 {
                    failure = Some(format!(
                        "the new file {} does not parse: {}",
                        file.path,
                        lang::first_error_summary(&content)
                    ));
                    break 'files;
                }
                format_clean.entry(file.path.clone()).or_insert(true);
                snapshot.record(relative)?;
                write_file(&pf.root, relative, &content)?;
                previous.insert(format!("{}::<file>", file.path), content.clone());
                ui::ok(&format!("created {}", file.path));

                if let Some(declaration) = lang::module_declaration(&pf.root, relative) {
                    let parent_absolute = pf.root.join(&declaration.parent);
                    let parent_source = std::fs::read_to_string(&parent_absolute)
                        .with_context(|| format!("failed to read {}", parent_absolute.display()))?;
                    if lang::module_missing(&parent_source, &declaration.module) {
                        let updated = lang::add_module(&parent_source, &declaration.module)?;
                        let parent_key = declaration.parent.to_string_lossy().replace('\\', "/");
                        format_clean.entry(parent_key).or_insert_with(|| {
                            lang::rustfmt_clean(&parent_absolute, &project.edition)
                        });
                        snapshot.record(&declaration.parent)?;
                        write_file(&pf.root, &declaration.parent, &updated)?;
                        ui::ok(&format!(
                            "declared `mod {};` in {}",
                            declaration.module,
                            declaration.parent.display()
                        ));
                    }
                } else if file.path.starts_with("src/") {
                    ui::warn(&format!(
                        "{} was created but is not linked into the module tree",
                        file.path
                    ));
                }
                continue;
            }

            for symbol in &file.symbols {
                let absolute = pf.root.join(relative);
                let source = std::fs::read_to_string(&absolute)
                    .with_context(|| format!("failed to read {}", absolute.display()))?;
                let resolved = match lang::resolve(&source, symbol) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        failure = Some(format!("{error:#}"));
                        break 'files;
                    }
                };
                let context = file_context(&source, resolved.start_byte);
                let key = format!("{}::{}", file.path, symbol);
                let edit = agent::edit_symbol(
                    &client,
                    &agent::EditContext {
                        task: &task,
                        plan: &plan.plan,
                        file: &file.path,
                        symbol: &resolved.qualified,
                        kind: resolved.kind,
                        symbol_source: &resolved.source,
                        file_context: &context,
                        diagnostics: &diagnostics,
                        previous: previous.get(&key).map(String::as_str),
                        attempt,
                        max_attempts,
                    },
                )
                .await?;
                log.write_json(
                    &format!(
                        "attempt-{attempt}/edit-{}-{}.json",
                        slug(&file.path),
                        slug(symbol)
                    ),
                    &edit,
                );
                if edit.give_up {
                    failure = Some(format!(
                        "the editor gave up on {}::{}: {}",
                        file.path, symbol, edit.thought
                    ));
                    break 'files;
                }
                if edit.truncated {
                    failure = Some(format!(
                        "the replacement for `{}` was cut off (the model hit the output limit); it must contain the complete symbol.",
                        resolved.qualified
                    ));
                    break 'files;
                }
                if let Some(problem) = lang::syntax_problem(&edit.new_code) {
                    failure = Some(format!(
                        "the replacement for `{}` does not parse on its own: {problem}",
                        resolved.qualified
                    ));
                    break 'files;
                }

                let before_errors = lang::error_count(&source)?;
                let updated =
                    match lang::apply(&source, &resolved, &edit.new_code, &edit.new_imports) {
                        Ok(updated) => updated,
                        Err(error) => {
                            failure = Some(format!("{error:#}"));
                            break 'files;
                        }
                    };
                let after_errors = lang::error_count(&updated)?;
                if after_errors > before_errors {
                    failure = Some(format!(
                        "the replacement for `{}` introduces syntax errors ({} -> {}): {}",
                        resolved.qualified,
                        before_errors,
                        after_errors,
                        lang::first_error_summary(&updated)
                    ));
                    break 'files;
                }

                format_clean
                    .entry(file.path.clone())
                    .or_insert_with(|| lang::rustfmt_clean(&absolute, &project.edition));
                snapshot.record(relative)?;
                write_file(&pf.root, relative, &updated)?;
                previous.insert(key, edit.new_code.clone());
                ui::ok(&format!("patched {}::{}", file.path, resolved.qualified));
            }
        }

        if let Some(reason) = failure {
            diagnostics = reason.clone();
            snapshot.restore_all()?;
            log.write(&format!("attempt-{attempt}/rejected.txt"), &reason);
            ui::warn(&format!("attempt rejected: {reason}"));
            continue;
        }

        for relative in snapshot.touched() {
            let key = relative.to_string_lossy().replace('\\', "/");
            if format_clean.get(&key).copied().unwrap_or(false) {
                let absolute = pf.root.join(&relative);
                if let Err(error) = lang::rustfmt(&absolute, &project.edition) {
                    ui::warn(&format!("rustfmt failed: {error:#}"));
                }
            }
        }

        ui::stage(&format!("verify attempt {attempt}"));
        let result = verify::run_all(
            &pf.root,
            &[check_command.clone(), test_command.clone()],
            Duration::from_secs(600),
        )
        .await;
        log.write(&format!("attempt-{attempt}/verify.txt"), &result.summary);
        ui::print_verify(&result);

        if result.success {
            success = true;
            break;
        }

        diagnostics = result.summary.clone();
        snapshot.restore_all()?;
        ui::warn("verification failed; rolling back and retrying");
    }

    if !success {
        ui::error(&format!(
            "could not produce a green build after {max_attempts} attempts"
        ));
        if !diagnostics.trim().is_empty() {
            ui::detail(&diagnostics);
        }
        return Ok(3);
    }

    let touched: Vec<String> = snapshot
        .touched()
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect();
    let diff = build_diff(&pf.root, &touched)?;
    let stat = build_stat(&pf.root, &touched)?;
    log.write("final.diff", &diff);
    log.write("final.stat", &stat);

    ui::stage("review");
    ui::print_diff(&stat, &diff);
    print_risk_notes(&pf.root, &snapshot, &touched);

    if cli.dry_run {
        snapshot.restore_all()?;
        ui::info("dry run: changes discarded");
        return Ok(0);
    }

    let mut message = match agent::commit_message(&client, &task, &stat, &diff).await {
        Ok(message) => message,
        Err(error) => {
            ui::warn(&format!(
                "commit message generation failed ({error:#}); using a fallback"
            ));
            fallback_message(&task)
        }
    };

    if !cli.yes {
        ui::info(&format!("proposed commit message:\n\n{message}\n"));
        let replacement = ui::ask("commit message (enter to accept, or type a replacement)")?;
        if !replacement.is_empty() {
            message = replacement;
        }
    }

    if !ui::confirm("commit these changes?", false, cli.yes)? {
        snapshot.restore_all()?;
        ui::info("changes discarded");
        return Ok(2);
    }

    let existing: Vec<String> = touched
        .into_iter()
        .filter(|path| pf.root.join(path).exists())
        .collect();
    git::stage(&pf.root, &existing)?;
    match git::commit(&pf.root, &message) {
        Ok(()) => {
            ui::ok(&format!("committed: {}", first_line(&message)));
            Ok(0)
        }
        Err(error) => {
            ui::error(&format!("{error:#}"));
            ui::warn(
                "changes are left staged; fix the issue and commit manually, or run `git restore --staged . && git checkout -- .`",
            );
            Ok(1)
        }
    }
}

/// A small existing file used as a style reference when creating a new one.
fn style_reference(root: &Path) -> String {
    for candidate in ["src/lib.rs", "src/main.rs"] {
        let path = root.join(candidate);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let truncated: String = text.chars().take(4000).collect();
            return truncated;
        }
    }
    String::new()
}

fn write_file(root: &Path, relative: &Path, content: &str) -> Result<()> {
    let absolute = root.join(relative);
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&absolute, content)
        .with_context(|| format!("failed to write {}", absolute.display()))
}

/// Apply the plan's deterministic renames to one block of code (used for
/// newly created files, which the rename pass has not seen yet).
fn apply_plan_renames(code: &str, renames: &[Rename]) -> Result<String> {
    let mut content = code.to_string();
    for rename in renames {
        content = lang::rename_occurrences(&content, &rename.from, &rename.to)?.0;
    }
    Ok(content)
}

/// Rewrite every occurrence of each planned rename in every `.rs` file under
/// `root`. Files already on the snapshot are skipped; the rest are recorded
/// before their first write so a failed attempt rolls back cleanly.
fn apply_renames(
    root: &Path,
    renames: &[Rename],
    snapshot: &mut Snapshot,
) -> Result<Option<String>> {
    if renames.is_empty() {
        return Ok(None);
    }
    let mut touched = Vec::new();
    for relative in rust_files(root) {
        let absolute = root.join(&relative);
        let source = match std::fs::read_to_string(&absolute) {
            Ok(source) => source,
            Err(error) => {
                ui::warn(&format!("skipping {}: {error:#}", relative.display()));
                continue;
            }
        };
        let mut content = source.clone();
        let mut count = 0usize;
        for rename in renames {
            match lang::rename_occurrences(&content, &rename.from, &rename.to) {
                Ok((next, replaced)) => {
                    count += replaced;
                    content = next;
                }
                Err(error) => {
                    ui::warn(&format!(
                        "skipping {} for rename `{}`: {error:#}",
                        relative.display(),
                        rename.from
                    ));
                }
            }
        }
        if count == 0 {
            continue;
        }
        snapshot.record(&relative)?;
        write_file(root, &relative, &content)?;
        touched.push(relative.clone());
    }
    if touched.is_empty() {
        return Ok(None);
    }
    let names = touched
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(format!("renamed symbols in {names}")))
}

/// Every `.rs` file under `root`, sorted for determinism, skipping `target/`.
fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_filesize(Some(1_000_000))
        .build();
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => continue,
        };
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if normalized.starts_with("target/") || normalized.contains("/target/") {
            continue;
        }
        files.push(relative);
    }
    files.sort();
    files
}

fn file_context(source: &str, symbol_start: usize) -> String {
    // Small files are cheap to include in full, and seeing the tests is often
    // what lets a small model understand the intended behavior.
    if source.len() <= 12_000 {
        return source.to_string();
    }
    let prefix = &source[..symbol_start.min(source.len())];
    let lines: Vec<&str> = prefix.lines().collect();
    let start = lines.len().saturating_sub(60);
    let mut context = lines[start..].join("\n");
    if context.len() > 3000 {
        let mut cut = context.len() - 3000;
        while cut < context.len() && !context.is_char_boundary(cut) {
            cut += 1;
        }
        context = format!("…\n{}", &context[cut..]);
    }
    context
}

fn build_diff(root: &Path, touched: &[String]) -> Result<String> {
    let mut output = String::new();
    for path in touched {
        if git::is_tracked(root, path) {
            let diff = git::diff_tracked(root, std::slice::from_ref(path))?;
            output.push_str(&diff);
            if !diff.is_empty() && !diff.ends_with('\n') {
                output.push('\n');
            }
        } else {
            let absolute = root.join(path);
            if !absolute.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&absolute).unwrap_or_default();
            let lines: Vec<&str> = content.lines().collect();
            output.push_str(&format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n",
                lines.len()
            ));
            for line in lines {
                output.push('+');
                output.push_str(line);
                output.push('\n');
            }
        }
    }
    Ok(output)
}

fn build_stat(root: &Path, touched: &[String]) -> Result<String> {
    let tracked: Vec<String> = touched
        .iter()
        .filter(|path| git::is_tracked(root, path))
        .cloned()
        .collect();
    let mut parts = Vec::new();
    if !tracked.is_empty() {
        let stat = git::diff_stat(root, &tracked)?;
        if !stat.trim().is_empty() {
            parts.push(stat);
        }
    }
    for path in touched.iter().filter(|path| !git::is_tracked(root, path)) {
        let absolute = root.join(path);
        if absolute.exists() {
            let lines = std::fs::read_to_string(&absolute)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            parts.push(format!(" {path} | {lines} + (new file)"));
        }
    }
    Ok(parts.join("\n"))
}

fn print_risk_notes(root: &Path, snapshot: &Snapshot, touched: &[String]) {
    let mut notes = Vec::new();
    for path in touched {
        let relative = Path::new(path);
        let before = snapshot
            .original(relative)
            .map(|bytes| String::from_utf8_lossy(bytes).to_string())
            .unwrap_or_default();
        let after = std::fs::read_to_string(root.join(relative)).unwrap_or_default();
        let before_tests = count_tests(&before);
        let after_tests = count_tests(&after);
        if after_tests < before_tests {
            notes.push(format!(
                "{path}: test count dropped from {before_tests} to {after_tests}"
            ));
        }
        if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
            notes.push(format!("{path}: dependency manifest changed"));
        }
        if path.starts_with("tests/") || path.contains("/tests/") {
            notes.push(format!("{path}: test file changed"));
        }
    }
    if notes.is_empty() {
        ui::info("risk notes: none");
    } else {
        ui::warn("risk notes:");
        for note in notes {
            ui::detail(&note);
        }
    }
}

fn count_tests(source: &str) -> usize {
    source
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("#[test]")
                || trimmed.starts_with("#[tokio::test")
                || trimmed.starts_with("#[rstest]")
        })
        .count()
}

fn fallback_message(task: &str) -> String {
    let line = first_line(task);
    if line.is_empty() {
        "chore: apply gig changes".to_string()
    } else {
        format!("chore: {line}")
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().to_string()
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character);
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    output.trim_matches('-').chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_is_the_default_mode() {
        assert_eq!(resolve_mode(false, false, false), (RunMode::Plan, false));
    }

    #[test]
    fn build_flag_selects_build_mode() {
        assert_eq!(resolve_mode(false, true, false), (RunMode::Build, false));
    }

    #[test]
    fn dry_run_implies_build_mode() {
        assert_eq!(resolve_mode(false, false, true), (RunMode::Build, false));
        assert_eq!(resolve_mode(false, true, true), (RunMode::Build, false));
    }

    #[test]
    fn plan_wins_and_reports_conflicts() {
        assert_eq!(resolve_mode(true, true, false), (RunMode::Plan, true));
        assert_eq!(resolve_mode(true, false, false), (RunMode::Plan, false));
        assert_eq!(resolve_mode(true, false, true), (RunMode::Plan, false));
    }

    #[test]
    fn applies_renames_to_all_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/model.rs"),
            "pub struct OllamaClient { base: String }\nimpl OllamaClient {\n    fn new() -> Self { Self { base: String::new() } }\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "use crate::model::OllamaClient;\npub fn make() -> OllamaClient { OllamaClient { base: String::new() } }\n",
        )
        .unwrap();
        // Generated artifacts must be skipped.
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(
            dir.path().join("target/debug/gen.rs"),
            "pub struct OllamaClient {}\n",
        )
        .unwrap();

        let renames = vec![Rename {
            from: "OllamaClient".to_string(),
            to: "LocalModelServerClient".to_string(),
        }];
        let mut snapshot = Snapshot::new(dir.path());
        let summary = apply_renames(dir.path(), &renames, &mut snapshot)
            .unwrap()
            .unwrap();

        assert!(summary.contains("src/lib.rs"), "{summary}");
        assert!(summary.contains("src/model.rs"), "{summary}");
        assert!(!summary.contains("target/debug/gen.rs"), "{summary}");

        let model = std::fs::read_to_string(dir.path().join("src/model.rs")).unwrap();
        assert!(model.contains("pub struct LocalModelServerClient"));
        assert!(model.contains("impl LocalModelServerClient"));
        assert!(!model.contains("OllamaClient"));

        let lib = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        assert!(lib.contains("use crate::model::LocalModelServerClient;"));
        assert!(lib.contains("LocalModelServerClient { base:"));
        assert!(!lib.contains("OllamaClient"));

        let generated = std::fs::read_to_string(dir.path().join("target/debug/gen.rs")).unwrap();
        assert!(generated.contains("OllamaClient"));

        // The snapshot records the renamed files so rollback restores them.
        let touched = snapshot.touched();
        assert!(touched.contains(&PathBuf::from("src/lib.rs")));
        assert!(touched.contains(&PathBuf::from("src/model.rs")));
    }

    #[test]
    fn rename_pass_is_a_noop_without_renames() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            apply_renames(dir.path(), &[], &mut Snapshot::new(dir.path()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn applies_renames_to_new_files() {
        let code = "pub struct OllamaClient { base: String }\nimpl OllamaClient {\n    fn new() -> Self { Self { base: String::new() } }\n}\n";
        let renamed = apply_plan_renames(
            code,
            &[Rename {
                from: "OllamaClient".to_string(),
                to: "LocalModelServerClient".to_string(),
            }],
        )
        .unwrap();
        assert!(renamed.contains("pub struct LocalModelServerClient"));
        assert!(renamed.contains("impl LocalModelServerClient"));
        assert_eq!(lang::error_count(&renamed).unwrap(), 0);
    }
}
