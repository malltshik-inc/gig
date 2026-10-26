# Technical Specification: Local Agentic Developer Assistant (`gig`)

## 1. Executive Summary & Core Objectives

`gig` is a terminal-based AI developer assistant designed to run fully locally using local models (such as `qwen2.5-coder:7b-instruct` via Ollama) on constrained hardware setups (e.g., consumer GPUs with 8 GB VRAM).

The application provides an interactive, terminal user interface (TUI/ratatui) experience similar to OpenCode or Aider, enabling a complete inner-loop development workflow: inspecting project code, executing build and test commands, modifying existing codebases safely, and iterating on compilation failures.

### Key Goals

* **Reliable Local Tool Use:** Bypasses unstructured text tool-calling limitations of small models by leveraging logit-level JSON Schema sampling at the API endpoint.

* **Precision Code Editing:** Avoids fragile line-number diff calculation (such as standard unified git diffs) by utilizing Abstract Syntax Tree (AST) node parsing and replacement.

* **Context Preservation:** Prevents context window exhaustion (staying well under 16k tokens) through aggressive terminal output sanitization and stateless sub-agent context isolation.

## 2. Core Architectural Principles

### 2.1 Schema-Enforced Structured Generation

Rather than relying on natural language parsing or standard model-driven function calling, `gig` forces Ollama to output valid JSON matching an exact operational schema on every turn. This guarantees that the model produces predictable, executable actions (such as reading a file, replacing a function, or running a command) without emitting raw conversational text or malformed escape sequences.

### 2.2 AST Symbol-Level Code Mutability

Smaller models struggle to maintain precise whitespace and line-offset arithmetic when generating unified diffs (`@@ -x,y +a,b @@`).

`gig` delegates target resolution and patch application to a Tree-Sitter parsing engine:

1. The user/agent specifies a target source file and symbol name (e.g., a function or struct identifier).

2. The harness parses the source file into a Concrete Syntax Tree (CST/AST) and resolves the exact byte bounds of the target symbol.

3. The LLM returns *only* the new source code for that specific node.

4. The harness replaces the old byte range with the new code string and re-parses the file locally to ensure syntax validity before writing to disk.

### 2.3 Sub-Agent Context Isolation & Context Branching

Executing trial-and-error operations (e.g., attempting speculative bug fixes or generating targeted unit tests) directly in the main conversation history quickly degrades model performance due to context bloat.

When a multi-step sub-task or diagnostic fix is needed, the system branches off a lightweight **Sub-Agent**:

* **Fresh Context:** The sub-agent operates with a focused system prompt containing *only* the affected AST symbol and the specific error diagnostic.

* **Bounded Retries:** The sub-agent iterates internally (e.g., max 3 attempts) until the code parses cleanly and tests pass.

* **Summary Rollup:** Once complete (or upon reaching maximum retries), the sub-agent terminates. Its internal conversation history and intermediate raw failure logs are discarded. Only a single high-level summary string is appended to the Main Agent's primary context window.

### 2.4 Diagnostic Output Filtering

Standard build tool outputs (like `cargo test` or compiler backtraces) can span thousands of lines, consuming massive token budgets. `gig` intercepts stdout/stderr streams from local tooling and converts them into structured diagnostics (e.g., parsing structured compiler messages to isolate strictly `error` and `warning` levels, or extracting panic locations), feeding only clean, condensed error summaries back to the LLM.

## 3. System Components & Data Flow

```
                     ┌──────────────────────────────────────┐
                     │          TUI / Terminal Loop         │
                     │  (Interactive Input, Log Output)     │
                     └──────────────────┬───────────────────┘
                                        │ User Task Input
                                        ▼
                     ┌──────────────────────────────────────┐
                     │              Main Agent              │
                     │  (Main History & Operational Loop)   │
                     └──────────────────┬───────────────────┘
                                        │
           ┌────────────────────────────┼────────────────────────────┐
           ▼                            ▼                            ▼
┌─────────────────────┐      ┌────────────────────┐       ┌────────────────────┐
│   Ollama Client     │      │   AST Engine       │       │ Command Executor   │
│  (JSON API Driver)  │      │   (Tree-Sitter)    │       │ (Process Runner)   │
└─────────────────────┘      └────────────────────┘       └────────────────────┘
                                        │
                                 Delegate Fix/Test
                                        ▼
                     ┌──────────────────────────────────────┐
                     │        Sub-Agent Task Runner         │
                     │ (Isolated Context Branch & Retries)  │
                     └──────────────────────────────────────┘
```

## 4. Detailed Component Design & Responsibilities

### 4.1 Terminal User Interface (TUI)

* **User Experience:** Provides a multi-pane console application. Features an active conversation view, a live execution/log viewer, and a status bar indicating current LLM token usage and sub-agent state.

* **Input Routing:** Captures user prompts and forwards them to the Main Agent event loop. Supports slash commands (e.g., `/reset`, `/run`, `/quit`).

### 4.2 Main Agent Event Loop & State Machine

* **State Management:** Manages the primary conversation history stack.

* **Action Resolution:** Receives structured JSON responses from Ollama and dispatches tasks to the appropriate sub-system:

  * **File Read:** Reads target file content and appends it to context.

  * **AST Symbol Replacement:** Invokes the AST Engine to modify code.

  * **Command Execution:** Triggers shell operations via the Command Executor.

  * **Spawn Sub-Agent:** Offloads a speculative fix or diagnostic task to a child runner.

  * **Task Completion:** Summarizes work completed and returns control to the user.

### 4.3 Ollama API Client

* **HTTP Transport:** Handles asynchronous communication with Ollama's endpoints (`/api/chat`).

* **Schema Injection:** Injects the strict JSON format payload on every request, ensuring zero plain-text fallback responses.

* **Runtime Options:** Hardcodes inference options suitable for precise code manipulation:

  * Low temperature (`0.0` to `0.1`) for deterministic code generation.

  * Expanded context window limit (`num_ctx: 16384` or `32768`).

### 4.4 AST Engine (Tree-Sitter)

* **Symbol Resolution:** Parses source files for target programming languages (e.g., Rust) and locates specific functions, methods, or struct definitions by identifier name.

* **Byte-Range Replacement:** Calculates exact start and end byte indices for target nodes and substitutes updated code generated by the LLM.

* **Pre-Validation Guard:** Automatically parses the newly constructed file string *before* saving changes to disk. If Tree-Sitter reports syntax errors (`has_error`), the edit is rejected immediately, preventing corrupt code from reaching the filesystem.

### 4.5 Command Executor & Output Sanitizer

* **Command Dispatch:** Runs shell commands (e.g., `cargo test`, `cargo check`, `git status`) in subprocesses asynchronously.

* **Log Pruning:** Captures command output streams and filters them:

  * Intercepts standard compiler error streams.

  * Filters out passing test assertions, environment setup logs, and clean compile lines.

  * Retains only actionable compiler diagnostics (file path, line/column, error code, and error message).

### 4.6 Sub-Agent Task Engine

* **Context Creation:** Instantiates a minimal message array containing a specialized system prompt, the raw target symbol code, and the sanitized compiler error.

* **Isolation Loop:** Executes an internal loop allowing the sub-agent to propose fixes, run AST checks, and trigger test executions.

* **Self-Correction:** If a proposed edit fails AST validation or causes a new build error, the failure is appended *only* to the sub-agent's local context array for a second or third retry turn.

* **Teardown & Rollup:** Upon reaching a passing state or exhausting max retries, the sub-agent context array is completely deallocated. A concise text result is returned back to the Main Agent.

## 5. API Schema Specification

To ensure exact tool execution without function-calling model overhead, the client requires Ollama to adhere to the following schema structure for operational turns:

### Action Parameters

* `thought` (string): Short reasoning step explaining the choice of action.

* `action` (enum string): Must be one of `read_file`, `replace_symbol`, `run_command`, `spawn_subagent`, or `finish`.

* `file_path` (optional string): Target relative path for file operations.

* `target_symbol` (optional string): Name of the target function, struct, or impl block when executing AST symbol replacements.

* `new_code` (optional string): The updated code content for `target_symbol`.

* `command` (optional string): Shell command string when executing system commands.

* `subagent_task` (optional string): Detailed instructions provided to an isolated child context when spawning a sub-agent.

## 6. Execution Lifecycle Example: Automated Bug Fix

1. **User Request:** User prompts the TUI: *"Run tests and fix any failing Rust functions."*

2. **Main Agent Action 1:** Main Agent issues action `run_command` with `"cargo test"`.

3. **Command Execution & Sanitization:** The Command Executor runs `cargo test`, detects a compilation error in `fn process_data`, extracts the clean error message, and returns it to the Main Agent context.

4. **Main Agent Action 2:** Main Agent decides the fix requires trial-and-error. It issues action `spawn_subagent` targeting `process_data` in `src/processor.rs`.

5. **Sub-Agent Loop:**

   * **Sub-Turn A:** Sub-Agent receives *only* `fn process_data` source and the 4-line compiler error. It outputs updated code for `process_data`.

   * **Validation:** AST Engine validates syntax (Pass). Harness runs `cargo test` (Fail - logic error).

   * **Sub-Turn B:** Sub-Agent receives test failure reason in its isolated context, adjusts code, and outputs a revised `process_data`.

   * **Validation:** AST Engine validates syntax (Pass). Harness runs `cargo test` (Pass).

   * **Teardown:** Sub-Agent completes. Sub-Agent memory is deleted.

6. **Main Agent Action 3:** Main Agent receives message: *"Sub-agent successfully resolved compilation error in `process_data`."*

7. **Main Agent Action 4:** Main Agent outputs `finish` action with thought *"All tests are passing."* UI displays completion message to the user.