# gig

A local patch-and-commit agent for Rust projects. You give it a task; it plans, edits code through tree-sitter, runs `cargo check` and `cargo test`, shows you the diff, and commits only after you approve.

Built for small local models (Qwen2.5-Coder 7B via Ollama) on constrained hardware, not for autonomous cloud agents.

> `docs/SPEC.md` is the original conversational-agent design. The implementation follows the commit-generator pipeline described here.

## Requirements

- Rust 1.88+ (the crate uses edition 2024)
- `git`
- [Ollama](https://ollama.com) running locally
- A code model, for example:
  ```sh
  ollama pull qwen2.5-coder:7b-instruct
  ```
- A Cargo project with a clean git working tree

## Build

```sh
cargo build --release
# binary: target/release/gig
```

## Usage

```sh
# From the repository you want to change
gig "Fix the login timeout bug and make the tests pass"

# Point at another repository
gig --dir ../my-project "Add a subtract function to src/math.rs"

# Plan only, no edits
gig --plan-only "Refactor process_data to return Result"

# Run the full pipeline, show the diff, then restore the tree
gig --dry-run "..."

# Non-interactive: approve the plan and commit automatically
gig --yes "..."

# Prompt from stdin
printf '%s\n' "Fix the failing test in src/auth.rs" | gig --dir .
```

### Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | `.` | Repository directory |
| `--model <NAME>` | `qwen2.5-coder:7b-instruct` | Ollama model |
| `--num-ctx <N>` | `16384` | Context window passed to Ollama |
| `--max-attempts <N>` | `3` | Edit/verify retries before giving up |
| `--max-files <N>` | `6` | Maximum files the planner may touch |
| `--yes` | off | Skip all confirmation prompts |
| `--plan-only` | off | Print the plan and stop |
| `--dry-run` | off | Full pipeline, never commit, restore afterwards |
| `--no-baseline` | off | Skip the baseline `cargo check`/`cargo test` run |
| `--verbose` | off | Print raw model requests and responses |

## Pipeline

1. **Preflight** — verifies git, a clean tracked tree, `user.name`/`user.email`, the Ollama model, and handles repositories with no commits yet.
2. **Detect** — reads `Cargo.toml`, package name, and edition. Rust only in this build.
3. **Baseline** — runs `cargo check` and `cargo test` on the clean tree so pre-existing failures are visible to the planner.
4. **Repo map** — gitignore-aware walk, tree-sitter symbol extraction, lexical ranking by task keywords.
5. **Plan** — one schema-constrained Ollama call producing files, symbols, intent, and a short plan.
6. **Plan gate** — shows the plan and asks to proceed (skipped with `--yes`).
7. **Edit loop** — one model call per symbol. The harness resolves the exact byte range, preserves attributes and doc comments, inserts new imports, re-parses to reject syntax errors, and formats with `rustfmt` when the file was already clean. New files are generated whole and linked into the module tree deterministically (creating `src/math.rs` inserts `mod math;` into `src/lib.rs` or the parent module).
8. **Verify** — `cargo check` then `cargo test`, with structured Cargo diagnostics and test-failure extraction. Failures are fed back to the editor, up to `--max-attempts`.
9. **Review** — diff, stat, risk notes, and a commit confirmation.
10. **Commit** — a generated Conventional Commit message (editable), staging only the files the agent touched.

## Safety model

- The model **cannot run shell commands**. The verifier runs a fixed command set (`cargo check`, `cargo test`).
- A dirty tracked working tree is refused before any model call.
- Every touched file is snapshotted in memory before its first write. Failed attempts, rejection, and `--dry-run` restore it. This works even on a repository with no commits, where `git checkout -- .` has nothing to restore.
- Model-supplied paths are confined to the repository root; absolute paths and `..` are rejected.
- New module files are linked by the harness, not by the model: it only writes `mod name;` when the declaration is absent, and the parent file is snapshotted and shown in the diff.
- Only touched files are staged, never `git add -A`.
- Test counts are compared before and after; a drop is surfaced in the risk notes.
- Each run writes a transcript to `.git/gig/runs/<timestamp>-<pid>/` (plan, edits, verification output, final diff). It is inside `.git`, so it is never committed.

## Limits

- **Rust/Cargo only.** The extension point is the `lang` module; Python/TypeScript support is future work.
- **A red baseline requires a fully green result.** The agent cannot distinguish "fix only this failure" from "fix everything" yet.
- **No Ctrl-C rollback.** Killing the process can leave edits in place; use `--dry-run` or review the diff.
- **Warnings do not fail verification.**
- **Test tampering is warned about, not blocked.** Review the diff.
- **Lexical retrieval only.** No embeddings in this version.
- **No TUI, no sub-agents.** The pipeline is serial because it shares one local model.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

The unit tests cover tree-sitter symbol resolution and replacement, indentation and attribute preservation, import insertion, path traversal, Cargo diagnostic parsing, test-failure extraction, repo-map ranking, and snapshot rollback.
