# Technical Specification: `gig`

Status: implemented, v0.1.0.

`gig` is a local, non-interactive patch-and-commit agent for Rust projects. You give it a task, a repository, and an Ollama model. It plans the change, applies it through tree-sitter, verifies it with Cargo, shows the diff, and commits only after approval.

This document describes the system as built. It replaces the earlier conversational/TUI design; that design remains in git history.

## 1. Goals

- **Run fully locally.** No cloud API, no telemetry, no network calls except to the local Ollama endpoint.
- **Fit constrained hardware.** One model at a time, serialized stages, default `num_ctx` of 16384, suitable for 8 GB VRAM.
- **Edit precisely.** Symbol-level replacement through tree-sitter rather than model-generated unified diffs or line numbers.
- **Fail safely.** The model cannot execute commands; every touched file is snapshotted; a failed attempt is rolled back.
- **Keep the human in the loop.** A plan gate before edits and a review gate before commit.

## 2. Non-goals

- No TUI, no chat loop, no slash commands, no token-usage status bar.
- No sub-agent framework. Every stage is a one-shot call; only the editor retries.
- No model-driven shell execution. The verifier runs a fixed command set.
- No embeddings or vector store. Retrieval is lexical.
- No languages other than Rust in this version.

## 3. Pipeline

```
CLI prompt
   │
   ▼
preflight ──► detect ──► baseline ──► repo map
   │                                   │
   │                                   ▼
   │                                 plan ──► clarification? ──► re-plan (once)
   │                                   │
   │                                   ▼
   │                              plan gate (human)
   │                                   │
   │                                   ▼
   │                          edit loop (LLM, per symbol)
   │                                   │
   │                                   ▼
   │                             verify (cargo check, cargo test)
   │                                   │ fail ──► rollback, retry
   │                                   ▼
   │                             review gate (human, diff)
   │                                   │
   │                                   ▼
   └─────────────────────────────► commit (LLM message + approval)
```

All stages are serial and share a single model. The only loop with model calls is edit → verify.

## 4. Stages

### 4.1 CLI and input

- Task prompt comes from positional arguments (joined with spaces) or stdin when no arguments are given and stdin is not a terminal.
- An empty prompt is an error.
- The repository defaults to the current directory.

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | `.` | Repository directory |
| `--model <NAME>` | `qwen2.5-coder:7b-instruct` | Ollama model |
| `--num-ctx <N>` | `16384` | Context window sent to Ollama |
| `--max-attempts <N>` | `3` | Edit/verify attempts |
| `--max-files <N>` | `6` | Maximum files the planner may touch |
| `--yes` | off | Skip plan, message, and commit confirmations |
| `--plan-only` | off | Print the sanitized plan and exit |
| `--dry-run` | off | Run everything, then restore the tree and exit |
| `--no-baseline` | off | Skip the baseline verification |
| `--verbose` | off | Print raw model requests and replies |

### 4.2 Preflight

Deterministic; the only Ollama request is the model availability check.

1. `git --version` must succeed.
2. The directory is canonicalized and resolved to the repository toplevel with `git rev-parse --show-toplevel`.
3. `git status --porcelain` must contain no tracked modifications or staged changes. Untracked files (`??`) are allowed.
4. `HEAD` is checked with `git rev-parse --verify --quiet HEAD`. An unborn HEAD is allowed; rollback uses snapshots, not git.
5. The current branch and absolute git directory are recorded.
6. Missing `user.name` / `user.email` produces a warning, not a failure.
7. The configured model must appear in Ollama's `/api/tags` list. Missing models produce the installed list and a `ollama pull` hint.

### 4.3 Project detection

- Rust only: `Cargo.toml` must exist at the repository root.
- The package `name` and `edition` are read from the `[package]` table with a line-oriented parser. Missing edition falls back to `2021`.
- Absence of `Cargo.toml` is a hard error with a "Rust/Cargo only" message.

### 4.4 Baseline verification

Runs on the clean tree before planning:

- `cargo check --message-format=json`
- `cargo test --quiet`

Both must pass for a green baseline. A red baseline is not fatal: the structured summary is passed to the planner so pre-existing failures are visible. Because the final gate requires both commands to pass, a red baseline effectively means "fix everything."

`--no-baseline` replaces the summary with `(skipped)`.

### 4.5 Repository map

- Walk with the `ignore` crate: hidden files and gitignored paths (`.gitignore`, global gitignore, git excludes) are skipped; files over 1 MB are skipped.
- Only `.rs` files outside `target/` are indexed.
- Each file is parsed with tree-sitter; the index records qualified symbol names and kinds.
- Lexical scoring per task token:
  - path contains token: `+8`
  - symbol name equals token: `+30`
  - symbol name contains token: `+12`
  - qualified name contains token: `+4`
  - path depth penalty: `-1` per `/`
- The top 400 files are kept; the renderer truncates to 12,000 characters.
- Rendering is file-oriented, one symbol per line with its kind, and states that indented symbols belong to the file above them.

### 4.6 Planning

One schema-constrained Ollama call.

Inputs: task, project name/language/edition, `--max-files`, baseline summary, rendered repo map.

Rules enforced by the prompt:

- Touch the fewest files possible; prefer editing existing symbols.
- Use only paths and symbols present in the map, unless creating a new file.
- The map is complete; do not invent files, modules, or symbols.
- Do not modify tests, CI, or test infrastructure; never delete or ignore a test.
- If the baseline is red, treat it as the bug; never change a test's expected value to match broken behavior.
- Do not add dependencies unless unavoidable.
- Ask for clarification only when no listed symbol could plausibly satisfy the task.
- Leave `verification` empty; the harness chooses commands.

If `needs_clarification` is true:

- With `--yes` or `--plan-only`, print the question and exit `2`.
- Otherwise ask one question on stdin, append the answer to the task, and re-plan once. A second clarification request is an error.

### 4.7 Plan sanitization and gate

Before display:

- Paths are normalized: backslashes become `/`, absolute paths and `..` are rejected, and existing paths must canonicalize inside the repository root.
- Files beyond `--max-files` are dropped.
- A nonexistent file is kept only if `create` is true and the path ends in `.rs`.
- For existing files, every symbol is normalized and resolved with tree-sitter. Unresolvable symbols are dropped with a warning. A file with no valid symbols is dropped.
- Duplicate file entries are merged and their symbol lists unioned.

An empty plan is an error. `--plan-only` prints the plan and exits `0`. Otherwise the user confirms; declining exits `2`.

### 4.8 Editing

Attempts run from `1` to `--max-attempts`. Each attempt walks the plan's files in order.

**New files**

- One model call returns the complete file contents.
- The response is checked with tree-sitter; any syntax error rejects the attempt.
- The file is written, then linked into the module tree deterministically:
  - `src/foo.rs` → `mod foo;` in `src/lib.rs`, else `src/main.rs`.
  - `src/a/b.rs` → `mod b;` in `src/a.rs`, else `src/a/mod.rs`.
  - `src/a/mod.rs` → `mod a;` in the parent module.
  - `src/lib.rs`, `src/main.rs`, and `src/bin/*` are not linked.
- The declaration is inserted only when absent, using the same import-insertion path as `use` lines. The parent file is snapshotted and appears in the diff.
- A created file under `src/` that is not a crate root and has no resolvable parent produces a warning.

**Existing symbols**

- The symbol is re-resolved against the current file contents.
- The model receives the task, plan, file, qualified symbol, kind, attempt number, current symbol (including attributes and doc comments), file context, prior diagnostics, and the previous rejected replacement.
- File context is the whole file when it is at most 12,000 bytes; otherwise the 60 lines before the symbol, truncated to 3,000 characters from the end.
- The replacement is cleaned of markdown fences, stripped of duplicated attribute/doc lines, dedented, and re-indented to the symbol's indentation.
- The old node's line range is replaced. Attributes and doc comments immediately above the node are outside that range and preserved.
- New `use` lines and `mod name;` declarations are inserted after the last existing `use`, inner attribute, or inner doc comment (`//!`, `/*!`), deduplicated against existing lines.
- The resulting file is re-parsed. If the error count increased, the attempt is rejected.
- The file is formatted with `rustfmt` only when it was already `rustfmt --check` clean before the edit.

**Failure handling**

A retryable rejection records a diagnostic, restores the snapshot, and starts the next attempt. Retryable rejections include: the editor giving up, a new file that fails to parse, an edit that introduces syntax errors, and a symbol that becomes unresolvable or ambiguous during the attempt.

A hard error aborts the run with exit `1` instead of consuming an attempt. This covers transport failures, model responses that do not match the editor schema (invalid JSON, missing fields), and an empty replacement.

After all files are written, files marked format-clean are formatted with `rustfmt`; formatting errors are warnings.

### 4.9 Verification and retry

- Commands run in order and short-circuit on the first failure:
  1. `cargo check --message-format=json`
  2. `cargo test --quiet`
- Timeouts: 600 s per command; on Windows the process tree is killed with `taskkill /T /F`.
- `cargo check` output is parsed as Cargo JSON (`reason: compiler-message`), keeping `error` and `warning` levels with file, line, column, code, and message. Duplicates are removed and the list is capped at 50. If no JSON is found, stderr/stdout lines starting with `error` or `warning:` are used.
- `cargo test` failures are summarized from `---- name stdout ----` sections, panic lines, assertion lines, and `test result:` lines, capped at 3,500 characters.
- The full summary is capped at 6,000 characters.
- On failure, the summary becomes the next attempt's diagnostics and the snapshot is restored. Exhausting all attempts exits `3`.

### 4.10 Review

- The diff is built from `git diff` for tracked files and a synthetic `new file` diff for created files.
- The stat combines `git diff --stat` with a line count for new files.
- Diff display is capped at 20,000 bytes.
- Risk notes are computed from the snapshot's original bytes versus the current file:
  - test count dropped (counts `#[test]`, `#[tokio::test`, `#[rstest]`),
  - `Cargo.toml` changed,
  - a file under `tests/` changed.
- `--dry-run` restores the snapshot and exits `0` before any commit message call.

### 4.11 Commit

- One model call is prompted for a Conventional Commit message (subject ≤ 72 characters, imperative, no trailing period).
- If generation fails, a fallback is used: `chore: <first line of task>`.
- Unless `--yes`, the message is shown and can be replaced on stdin.
- The user confirms; declining restores the snapshot and exits `2`.
- Only touched files that still exist are staged with `git add -- <paths>`, never `git add -A`.
- `git commit -m <message>` runs once. A commit failure leaves the changes staged and exits `1` with recovery instructions.

## 5. Model interface

### 5.1 Client

- Endpoint: `POST {OLLAMA_HOST}/api/chat`, default `http://127.0.0.1:11434`. A host without a scheme gets `http://`.
- Requests set `stream: false`, `format: <schema>`, `temperature`, `num_ctx`, `num_predict`, `repeat_penalty: 1.05`, `top_p: 0.9`.
- A 900 s request timeout applies.
- If the reply is not valid JSON, the client retries once, appending the invalid reply and a correction instruction. JSON is extracted from prose or fences by brace matching that ignores braces inside strings.

### 5.2 Schemas

Planner:

```json
{
  "thought": "string",
  "needs_clarification": false,
  "clarification": "string",
  "plan": "string",
  "files": [
    { "path": "string", "symbols": ["string"], "intent": "string", "create": false }
  ],
  "verification": ["string"]
}
```

Editor (used for both symbol replacement and new files):

```json
{
  "thought": "string",
  "give_up": false,
  "new_imports": ["string"],
  "new_code": "string"
}
```

Commit:

```json
{ "message": "string" }
```

Generation limits: planner 1,400 tokens at temperature 0.1; editor 4,096 tokens at 0.1; commit 300 tokens at 0.2.

## 6. AST engine

- Grammar: `tree-sitter-rust`.
- Indexed items: functions, structs, enums, unions, traits, type aliases, consts, statics, macros, impl blocks, trait methods, and inline modules (recursively).
- Resolution order: exact qualified name (`Type::method`, `module::function`), then short name, then `::`-suffix. Multiple matches produce an ambiguity error listing candidates.
- When several candidates share a name, impl-block entries are discarded if exactly one non-impl candidate remains.
- Replacement range starts at the node's line start (including indentation). Contiguous attributes, line comments, and block comments directly above the node, separated by at most one newline, are treated as a preserved prefix.
- If the model repeats prefix lines, they are removed from the replacement before applying.
- EOL style follows the source file (`\r\n` or `\n`).
- Pre-write validation counts tree-sitter error and missing nodes before and after; an increase rejects the edit.

## 7. Safety model

| Risk | Control |
| --- | --- |
| Model runs arbitrary commands | No command action exists; verifier runs a fixed command list |
| Editing a dirty tree | Preflight rejects tracked modifications and staged changes |
| Losing work on failure | In-memory snapshot of every touched file, restored on rejection, failed verification, `--dry-run`, or declined commit; works on an unborn HEAD |
| Path escape | Absolute paths and `..` rejected; existing paths canonicalized and checked against the repo root |
| Staging unrelated files | Only touched, existing paths are staged |
| Silent test deletion | Test count compared before/after and surfaced in risk notes |
| Debuggability | Full transcript under `.git/gig/runs/<timestamp>-<pid>/` |

Snapshot entries map repository-relative paths to `Option<Vec<u8>>`; `None` means the agent created the file and rollback deletes it. Entries are recorded once, before the first write, and restored in reverse order.

## 8. Run log

Written under the git directory, so it is never committed:

```
.git/gig/runs/<unix-seconds>-<pid>/
  task.txt
  baseline.txt
  repo-map.txt
  plan.json
  plan-2.json                (after clarification)
  attempt-<n>/edit-<file>-<symbol>.json
  attempt-<n>/create-<file>.json
  attempt-<n>/rejected.txt
  attempt-<n>/verify.txt
  final.diff
  final.stat
```

Log write failures are ignored; logging never aborts a run.

## 9. Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success, plan-only, or dry run |
| `1` | Error: preflight, model, I/O, or commit failure |
| `2` | Aborted by the user or plan rejected |
| `3` | Verification failed after all attempts |

## 10. Module map

| File | Responsibility |
| --- | --- |
| `src/main.rs` | CLI definition, prompt input, process exit |
| `src/pipeline.rs` | Stage orchestration, edit/verify loop, diff, review, commit |
| `src/preflight.rs` | Git state, clean-tree enforcement, model availability |
| `src/detect.rs` | Cargo project detection, name, edition |
| `src/repomap.rs` | Gitignore-aware walk, symbol index, lexical ranking, rendering |
| `src/lang.rs` | Tree-sitter parsing, symbol resolution, replacement, imports, modules, rustfmt |
| `src/agent.rs` | Planner/editor/commit prompts, plan sanitization, path confinement |
| `src/model.rs` | `ModelClient` trait, Ollama client, schemas, typed plan/edit, mock client |
| `src/verify.rs` | Fixed command runner, timeouts, Cargo JSON diagnostics, test-failure extraction |
| `src/snapshot.rs` | Original-bytes snapshot and rollback |
| `src/git.rs` | Git wrappers: status, diff, stat, stage, commit |
| `src/runlog.rs` | Run transcript |
| `src/ui.rs` | Terminal output, plan/diff/verification rendering, prompts |

## 11. Extension points

- **Languages:** `detect` selects the project, `lang` owns parsing and replacement, and `pipeline` owns the verify command list. The current implementation is Rust-specific; adding a language means adding a detector, an AST implementation, and a verify command set, which would first need a small trait boundary.
- **Models:** any client implementing `ModelClient` can replace Ollama; `MockClient` exists for deterministic tests.
- **Retrieval:** the repo map is isolated behind `repomap::build` and `RepoMap::render`, so an embedding-backed ranker can replace lexical scoring without touching the planner.

## 12. Limits

- Rust/Cargo only.
- A red baseline requires a fully green final result.
- No Ctrl-C rollback; killing the process can leave edits in place.
- Warnings do not fail verification.
- Test tampering is reported, not blocked.
- Lexical retrieval only.
- Serial execution; no concurrency.
- The planner may still propose plausible-but-invalid symbols; sanitization drops them, which can empty the plan.
- A malformed editor response aborts the run instead of consuming an attempt.

## 13. History

The original specification described a conversational TUI with a main-agent loop, sub-agents, and model-selected shell commands. That design was replaced by the deterministic pipeline above before v0.1.0. The earlier text is preserved in git history.
