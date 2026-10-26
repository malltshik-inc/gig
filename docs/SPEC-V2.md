# SPEC-V2 — Cascade of small model contexts

> **Status: proposal / alternative direction.** This describes a potential v2
> approach, not the implemented pipeline. The v1 pipeline is specified in
> `SPEC.md`. This document exists to record the idea so it can be picked up
> (or rejected) later.

## The problem

V1 failed because it asked a small local model (Qwen2.5-Coder 7B) to emit
long, precise, structured output in a single shot — most noticeably whole-file
rewrites of config files. That reliably produces malformed output, truncation,
and upstream aborts ("token repeat limit reached"). The harness parts of gig
(tree-sitter symbol edits, renames, verification, rollback) all worked; the
failures were always in the *single big generation* step.

The working assumption behind V2: a small model can be a reliable **component**
inside a deterministic pipeline as long as every model output is tiny (a small
edit + a short note), and the pipeline gives the model **persistent memory
across context windows**. A small model cannot hold a multi-step task in one
context, so we give it a handoff protocol instead.

## The cascade engine

One invocation, many contexts:

```
gig -b "find the failing test, fix it, add docs, and push"
  └─ plan + one-time approval
  └─ for context in 1..=MAX_CONTEXTS:
       1. load handoff state (task, plan, tail of history, current note, last failure)
       2. model does ONE small change + writes done/next/hint
       3. harness applies it, validates the note ("next" must name a real symbol),
          runs a fast gate (tree-sitter parse + incremental cargo check)
       4. ok   → record, update note, continue
          fail → append diagnostics to state, immediately spin the next context
       5. plan fully executed and green → strip in-file markers → full cargo test
          → commit → (--push)
```

Termination is either **success** (plan done, build green, single commit) or
**budget exhausted** (report where it stopped, leave state for inspection).

## Handoff state

Lives in `.git/gig/sessions/<taskId>/state.json` (inside `.git`, never
committed). Kept small: `history` is a bounded ring buffer of the last ~6
entries.

```json
{
  "task": "find the failing test, fix it, add docs, and push",
  "plan": ["run tests", "fix symbol X", "update README", "commit"],
  "steps": [{ "item": "fix symbol X", "done": false }],
  "history": [
    {
      "ctx": 1,
      "action": "edit src/x.rs::parse",
      "result": "ok",
      "note": { "done": "...", "next": "fix src/y.rs::validate", "hint": "..." }
    },
    {
      "ctx": 2,
      "action": "edit src/y.rs::validate",
      "result": "verify failed: E0308",
      "note": {}
    }
  ],
  "current_note": { "done": "...", "next": "retry src/y.rs::validate", "hint": "..." },
  "budget": { "used": 2, "max": 30 }
}
```

Each new context prompt = task + plan summary + the last few history entries +
current note + last failure diagnostics.

## Resume notes (short, schema'd)

Constraints that keep the handoff honest:

- `done` — optional, ≤ one short line.
- `next` — **required**, non-empty, must resolve to a real symbol/file in the
  repo (checked with tree-sitter / path existence). Rejected notes get re-asked
  within the same context budget.
- `hint` — optional, one short line.

Optionally the harness can pin an in-file marker comment
(`// gig:resume <taskId>`) at the exact spot the next context must continue
from; markers are stripped on completion and the build re-verified afterwards.

## Guardrails

- **Budget**: `--max-contexts N` (default 30, configurable).
- **Progress rule**: every context must tick a plan step or change the note;
  N consecutive identical states → abort ("spinning").
- **Stale markers**: `next` referencing a symbol that no longer resolves aborts
  with "stale handoff, re-plan".
- **Single final commit**; **push only with `--push`**.
- Every context transcript lands in `.git/gig/runs/` for review.

## Relationship to v1

Reuses unchanged: tree-sitter symbol resolution and replacement, crate-wide
renames, whole-file editing for non-Rust text files, verification
(`cargo check` + `cargo test`), snapshot rollback, run logging.

New pieces:

- `resume` field on the edit schema (`{done, next, hint}`).
- Context orchestration loop replacing the fixed attempt loop.
- Session state file + handoff prompt assembly.
- Note validation and in-file marker strip pass.
- `--max-contexts` and `--push` flags.

## Accepted tradeoffs

- Per-context `cargo check` adds wall-clock time (30 contexts ≈ minutes of
  incremental builds). This fits "budget model, batch/background" usage, not
  interactive use.
- A small model is worst at writing "what to do next"; note validation exists
  specifically because of that.
- Free-form docs generation stays low-risk: docs can be mediocre and the
  verifier still gates everything that matters.

## Open questions

- Default `--max-contexts` (25? 30? 50?).
- Whether `cargo check` runs per context or only at completion (a "fast gate"
  of tree-sitter parse + deferred full verify would be faster but riskier).
- Should the cascade pause for approval when it wants to create a file / touch
  a new module, or run fully autonomously after the initial plan approval?
- Push behaviour: `--push` flag vs. default push at completion.
