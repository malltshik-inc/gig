use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::lang;
use crate::model::{self, ChatOptions, CommitMessage, Edit, ModelClient, Plan, PlanFile};
use crate::ui;

pub struct PlanContext<'a> {
    pub task: &'a str,
    pub project_name: &'a str,
    pub edition: &'a str,
    pub repo_map: &'a str,
    pub baseline: &'a str,
    pub max_files: usize,
}

pub async fn plan(model: &dyn ModelClient, context: &PlanContext<'_>) -> Result<Plan> {
    let system = r#"You are the planning stage of gig, a local coding assistant that produces a reviewed patch and commit.
You do not edit files and you do not run commands.

Rules:
- Touch the fewest files possible. Prefer editing existing symbols over rewriting whole files.
- Use only paths and symbol names that appear in the repository map, unless create is true for a new file.
- The map is complete. Do not invent files, modules, or symbols that are not listed.
- Indented symbols belong to the file above them. A `mod tests` block inside a file is not a separate file.
- For methods use the qualified name, e.g. `Type::method`.
- Do not modify tests, CI, or test infrastructure to make a build pass. Never delete or ignore a test.
- If baseline verification is shown, treat it as the bug to fix. Never change a test's expected value to match broken behavior; the implementation must satisfy the existing tests.
- Do not add dependencies unless the task cannot be done without one.
- Prefer the single most specific matching symbol and proceed. Set needs_clarification only when no listed symbol could plausibly satisfy the task.
- Leave `verification` empty; the harness chooses the commands.
- Keep `plan` to a short numbered list of edits.
Respond with JSON only."#;

    let user = format!(
        "TASK:\n{}\n\nPROJECT:\nname: {}\nlanguage: rust\nedition: {}\nmax files: {}\n\nBASELINE VERIFICATION:\n{}\n\nREPOSITORY MAP (file paths with symbols and kinds):\n{}",
        context.task,
        context.project_name,
        context.edition,
        context.max_files,
        context.baseline,
        context.repo_map
    );

    let reply = model
        .chat_json(
            system,
            &user,
            model::planner_schema(),
            ChatOptions {
                max_tokens: 1400,
                temperature: 0.1,
            },
        )
        .await?;
    let plan: Plan = serde_json::from_value(reply.value)
        .context("the planner returned JSON that does not match the plan schema")?;
    Ok(plan)
}

pub struct EditContext<'a> {
    pub task: &'a str,
    pub plan: &'a str,
    pub file: &'a str,
    pub symbol: &'a str,
    pub kind: &'a str,
    pub symbol_source: &'a str,
    pub file_context: &'a str,
    pub diagnostics: &'a str,
    pub previous: Option<&'a str>,
    pub attempt: u32,
    pub max_attempts: u32,
}

pub async fn edit_symbol(model: &dyn ModelClient, context: &EditContext<'_>) -> Result<Edit> {
    let system = r#"You are the editing stage of gig. You receive exactly one target symbol and must return its complete replacement source.

Rules:
- Return the entire symbol: attributes, doc comments, signature, body, and closing brace.
- Preserve the existing signature and visibility unless the plan requires a change.
- Preserve behavior unrelated to the task.
- The task and plan are authoritative. If the intended behavior is stated, implement it even when the tests are not shown; the harness runs them after your edit.
- If new imports or module declarations are required, put complete `use` lines or `mod name;` lines in new_imports. Do not duplicate items that already exist.
- Do not wrap code in markdown fences. Do not put commentary inside new_code.
- Set give_up to true only when the required change is genuinely underdetermined.
- Never delete tests, add #[ignore], or weaken assertions.
Respond with JSON only."#;

    let mut user = format!(
        "TASK:\n{}\n\nPLAN:\n{}\n\nFILE: {}\nSYMBOL: {} ({})\nATTEMPT: {} of {}\n\nCURRENT SYMBOL:\n```rust\n{}\n```\n\nFILE CONTEXT (whole file when small, otherwise imports and nearby items):\n```rust\n{}\n```",
        context.task,
        context.plan,
        context.file,
        context.symbol,
        context.kind,
        context.attempt,
        context.max_attempts,
        context.symbol_source,
        context.file_context
    );

    if !context.diagnostics.trim().is_empty() {
        user.push_str(&format!(
            "\n\nVERIFICATION ERRORS FROM THE PREVIOUS ATTEMPT:\n{}",
            context.diagnostics
        ));
    }
    if let Some(previous) = context.previous
        && !previous.trim().is_empty()
    {
        user.push_str(&format!(
            "\n\nPREVIOUS REJECTED REPLACEMENT:\n```rust\n{previous}\n```"
        ));
    }
    user.push_str(&format!(
        "\n\nReturn the complete replacement for `{}`.",
        context.symbol
    ));

    let reply = model
        .chat_json(
            system,
            &user,
            model::editor_schema(),
            ChatOptions {
                max_tokens: 4096,
                temperature: 0.1,
            },
        )
        .await?;
    let mut edit: Edit = serde_json::from_value(reply.value)
        .context("the editor returned JSON that does not match the edit schema")?;
    edit.new_code = clean_code(&edit.new_code);
    if !edit.give_up && edit.new_code.trim().is_empty() {
        bail!(
            "the editor returned an empty replacement for `{}`",
            context.symbol
        );
    }
    Ok(edit)
}

pub struct NewFileContext<'a> {
    pub task: &'a str,
    pub plan: &'a str,
    pub file: &'a str,
    pub context: &'a str,
    pub diagnostics: &'a str,
    pub attempt: u32,
    pub max_attempts: u32,
}

pub async fn edit_new_file(model: &dyn ModelClient, context: &NewFileContext<'_>) -> Result<Edit> {
    let system = r#"You are the editing stage of gig. You generate the complete contents of one new Rust source file.

You have no tools and no filesystem access. The harness writes your `new_code` string to disk verbatim. Writing the code as text in `new_code` IS creating the file, so never refuse for lack of filesystem access.

Rules:
- Return the complete contents of the file in new_code.
- Put every import or `mod` declaration directly in new_code; leave new_imports empty.
- Do not wrap code in markdown fences. Do not add commentary.
- Match the project's edition and style. Include tests only if the task asks for them.
- If you cannot make progress, set give_up to true and explain why in thought.
Respond with JSON only."#;

    let mut user = format!(
        "TASK:\n{}\n\nPLAN:\n{}\n\nFILE: {} (new file)\nATTEMPT: {} of {}\n",
        context.task, context.plan, context.file, context.attempt, context.max_attempts
    );
    if !context.context.trim().is_empty() {
        user.push_str(&format!(
            "\nEXISTING FILE FOR STYLE REFERENCE:\n```rust\n{}\n```\n",
            context.context
        ));
    }
    if !context.diagnostics.trim().is_empty() {
        user.push_str(&format!(
            "\nVERIFICATION ERRORS FROM THE PREVIOUS ATTEMPT:\n{}",
            context.diagnostics
        ));
    }
    user.push_str(&format!(
        "\nWrite the complete contents of `{}` into new_code.",
        context.file
    ));

    let reply = model
        .chat_json(
            system,
            &user,
            model::editor_schema(),
            ChatOptions {
                max_tokens: 4096,
                temperature: 0.1,
            },
        )
        .await?;
    let mut edit: Edit = serde_json::from_value(reply.value)
        .context("the editor returned JSON that does not match the edit schema")?;
    edit.new_code = clean_code(&edit.new_code);
    if !edit.give_up && edit.new_code.trim().is_empty() {
        bail!("the editor returned an empty file for `{}`", context.file);
    }
    Ok(edit)
}

pub async fn commit_message(
    model: &dyn ModelClient,
    task: &str,
    stat: &str,
    diff: &str,
) -> Result<String> {
    let system = r#"You write git commit messages. Reply with JSON {"message": "..."} only.
Use Conventional Commits (feat:, fix:, refactor:, docs:, test:, chore:).
The subject line must be at most 72 characters, imperative mood, no trailing period.
Add a short body only when it explains why the change was made."#;

    let user = format!(
        "TASK:\n{}\n\nDIFF STAT:\n{}\n\nDIFF (truncated):\n{}",
        task,
        stat,
        truncate(diff, 6000)
    );

    let reply = model
        .chat_json(
            system,
            &user,
            model::commit_schema(),
            ChatOptions {
                max_tokens: 300,
                temperature: 0.2,
            },
        )
        .await?;
    let message: CommitMessage = serde_json::from_value(reply.value)
        .context("the model returned JSON that does not match the commit schema")?;
    let message = clean_commit_message(&message.message);
    if message.is_empty() {
        bail!("the model returned an empty commit message");
    }
    Ok(message)
}

fn clean_commit_message(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let body = match trimmed.find('\n') {
        Some(index) => &trimmed[index + 1..],
        None => return String::new(),
    };
    match body.find("```") {
        Some(end) => body[..end].trim().to_string(),
        None => body.trim().to_string(),
    }
}

pub fn sanitize_plan(mut plan: Plan, root: &Path, max_files: usize) -> Result<Plan> {
    let mut files: Vec<PlanFile> = Vec::new();

    for file in plan.files.into_iter().take(max_files.max(1)) {
        let relative = match normalize_relative(root, &file.path) {
            Ok(relative) => relative,
            Err(error) => {
                ui::warn(&format!("dropping `{}`: {error:#}", file.path));
                continue;
            }
        };
        let relative_string = relative.to_string_lossy().replace('\\', "/");
        let absolute = root.join(&relative);
        let exists = absolute.exists();

        if !exists {
            if !file.create {
                ui::warn(&format!(
                    "dropping `{relative_string}`: the file does not exist and the plan did not mark it as new"
                ));
                continue;
            }
            if !relative_string.ends_with(".rs") {
                ui::warn(&format!(
                    "dropping `{relative_string}`: this build can only create Rust files"
                ));
                continue;
            }
            files.push(PlanFile {
                path: relative_string,
                symbols: Vec::new(),
                intent: file.intent,
                create: true,
            });
            continue;
        }

        let source = std::fs::read_to_string(&absolute)
            .with_context(|| format!("failed to read {}", absolute.display()))?;
        let mut valid = Vec::new();
        for raw in &file.symbols {
            let symbol = normalize_symbol(raw);
            if symbol.is_empty() {
                continue;
            }
            match lang::resolve(&source, &symbol) {
                Ok(resolved) => {
                    if !valid.contains(&resolved.qualified) {
                        valid.push(resolved.qualified);
                    }
                }
                Err(error) => {
                    ui::warn(&format!(
                        "dropping symbol `{raw}` from `{relative_string}`: {error:#}"
                    ));
                }
            }
        }

        if valid.is_empty() {
            ui::warn(&format!(
                "dropping `{relative_string}`: the plan named no symbols that exist in the file"
            ));
            continue;
        }

        files.push(PlanFile {
            path: relative_string,
            symbols: valid,
            intent: file.intent,
            create: false,
        });
    }

    let mut merged: Vec<PlanFile> = Vec::new();
    for file in files {
        if let Some(existing) = merged.iter_mut().find(|entry| entry.path == file.path) {
            let mut symbols: BTreeSet<String> = existing.symbols.drain(..).collect();
            symbols.extend(file.symbols);
            existing.symbols = symbols.into_iter().collect();
            if !file.intent.trim().is_empty() {
                existing.intent = file.intent;
            }
        } else {
            merged.push(file);
        }
    }

    plan.files = merged;
    Ok(plan)
}

fn normalize_symbol(raw: &str) -> String {
    let mut symbol = raw.trim().trim_matches('`').trim().to_string();

    // Models sometimes echo the map's `path: symbol` or `path::symbol` form.
    let normalized_prefix = symbol.replace('\\', "/");
    if let Some((prefix, rest)) = normalized_prefix.split_once("::")
        && (prefix.contains('/') || prefix.contains(".rs"))
    {
        symbol = rest.trim().to_string();
    } else if let Some((prefix, rest)) = symbol.rsplit_once(':')
        && !prefix.ends_with(':')
        && (prefix.contains('/') || prefix.contains(".rs"))
    {
        symbol = rest.trim().to_string();
    }

    // Models sometimes echo the map's `name (kind)` form.
    if let Some(index) = symbol.rfind(" (")
        && symbol.ends_with(')')
    {
        symbol.truncate(index);
    }

    symbol
        .trim_start_matches("pub(crate) ")
        .trim_start_matches("pub ")
        .trim_start_matches("fn ")
        .trim_end_matches("()")
        .trim()
        .to_string()
}

pub fn normalize_relative(root: &Path, raw: &str) -> Result<PathBuf> {
    let raw = raw.trim().replace('\\', "/");
    if raw.is_empty() {
        bail!("empty path");
    }
    let path = Path::new(&raw);
    if path.is_absolute() {
        bail!("absolute paths are not allowed");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("path escapes the repository"),
        }
    }

    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let joined = root.join(path);
    let check = if joined.exists() {
        joined.canonicalize().ok()
    } else {
        joined
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
    };
    if let Some(canonical) = check
        && !canonical.starts_with(&root_canonical)
    {
        bail!("path escapes the repository");
    }
    Ok(path.to_path_buf())
}

fn clean_code(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let body = match trimmed.find('\n') {
        Some(index) => &trimmed[index + 1..],
        None => return String::new(),
    };
    let body = body.trim_end();
    if let Some(end) = body.find("```") {
        body[..end].trim_end().to_string()
    } else {
        body.to_string()
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        let root = std::env::temp_dir();
        assert!(normalize_relative(&root, "../etc/passwd").is_err());
        assert!(normalize_relative(&root, "src/main.rs").is_ok());
    }

    #[test]
    fn strips_markdown_fences() {
        assert_eq!(clean_code("```rust\nfn a() {}\n```"), "fn a() {}");
        assert_eq!(
            clean_code("```rust\nfn a() {}\n```\nextra prose"),
            "fn a() {}"
        );
        assert_eq!(clean_code("fn a() {}"), "fn a() {}");
        assert_eq!(clean_commit_message("```\nfix: a bug\n```"), "fix: a bug");
    }

    #[test]
    fn normalizes_symbol_names() {
        assert_eq!(normalize_symbol("pub fn process_data()"), "process_data");
        assert_eq!(normalize_symbol("`Type::method`"), "Type::method");
        assert_eq!(normalize_symbol("src/lib.rs:add"), "add");
        assert_eq!(normalize_symbol("src/lib.rs::add"), "add");
        assert_eq!(normalize_symbol("add (function)"), "add");
        assert_eq!(
            normalize_symbol("tests::adds_two_numbers (function)"),
            "tests::adds_two_numbers"
        );
    }
}
