use std::collections::BTreeSet;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct VerifyCommand {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub level: String,
    pub code: Option<String>,
    pub message: String,
}

pub struct CommandRun {
    pub label: String,
    pub success: bool,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
}

#[allow(dead_code)] // diagnostics/runs are kept for the run log and future UI
pub struct VerifyResult {
    pub success: bool,
    pub summary: String,
    pub diagnostics: Vec<Diagnostic>,
    pub runs: Vec<CommandRun>,
}

pub async fn run_all(root: &Path, commands: &[VerifyCommand], timeout: Duration) -> VerifyResult {
    let mut runs = Vec::new();
    let mut diagnostics = Vec::new();
    let mut success = true;

    for command in commands {
        let run = run_command(command, root, timeout).await;
        if !run.success {
            success = false;
        }
        if command.label.contains("check") || command.label.contains("clippy") {
            let parsed = parse_cargo_json(&run.stdout);
            if parsed.is_empty() && !run.success {
                diagnostics.extend(fallback_diagnostics(&run));
            } else {
                diagnostics.extend(parsed);
            }
        }
        runs.push(run);
        if !success {
            break;
        }
    }

    let summary = summarize(&runs, &diagnostics);
    VerifyResult {
        success,
        summary,
        diagnostics,
        runs,
    }
}

async fn run_command(command: &VerifyCommand, root: &Path, timeout: Duration) -> CommandRun {
    let start = Instant::now();
    let child = match Command::new(&command.program)
        .args(&command.args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return CommandRun {
                label: command.label.clone(),
                success: false,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("failed to spawn {}: {error}", command.program),
                duration_ms: start.elapsed().as_millis(),
            };
        }
    };

    let pid = child.id();
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => CommandRun {
            label: command.label.clone(),
            success: output.status.success(),
            timed_out: false,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            duration_ms: start.elapsed().as_millis(),
        },
        Ok(Err(error)) => CommandRun {
            label: command.label.clone(),
            success: false,
            timed_out: false,
            stdout: String::new(),
            stderr: format!("failed to wait for {}: {error}", command.label),
            duration_ms: start.elapsed().as_millis(),
        },
        Err(_) => {
            #[cfg(windows)]
            if let Some(pid) = pid {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .output();
            }
            CommandRun {
                label: command.label.clone(),
                success: false,
                timed_out: true,
                stdout: String::new(),
                stderr: format!(
                    "{} exceeded the {}s timeout",
                    command.label,
                    timeout.as_secs()
                ),
                duration_ms: start.elapsed().as_millis(),
            }
        }
    }
}

#[derive(Deserialize)]
struct CargoLine {
    reason: Option<String>,
    message: Option<CargoMessage>,
}

#[derive(Deserialize)]
struct CargoMessage {
    level: String,
    message: String,
    code: Option<CargoCode>,
    spans: Option<Vec<CargoSpan>>,
}

#[derive(Deserialize)]
struct CargoCode {
    code: String,
}

#[derive(Deserialize)]
struct CargoSpan {
    file_name: String,
    line_start: usize,
    column_start: usize,
    is_primary: bool,
}

pub fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen = BTreeSet::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let parsed: CargoLine = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        if parsed.reason.as_deref() != Some("compiler-message") {
            continue;
        }
        let message = match parsed.message {
            Some(message) => message,
            None => continue,
        };
        if message.level != "error" && message.level != "warning" {
            continue;
        }
        let span = message.spans.as_ref().and_then(|spans| {
            spans
                .iter()
                .find(|span| span.is_primary)
                .or_else(|| spans.first())
        });
        let diagnostic = Diagnostic {
            file: span.map(|span| span.file_name.clone()),
            line: span.map(|span| span.line_start),
            column: span.map(|span| span.column_start),
            level: message.level,
            code: message.code.map(|code| code.code),
            message: message.message,
        };
        let key = format_diagnostic(&diagnostic);
        if seen.insert(key) {
            diagnostics.push(diagnostic);
        }
        if diagnostics.len() >= 50 {
            break;
        }
    }
    diagnostics
}

fn fallback_diagnostics(run: &CommandRun) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let text = format!("{}\n{}", run.stderr, run.stdout);
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("error") || trimmed.starts_with("warning:") {
            diagnostics.push(Diagnostic {
                file: None,
                line: None,
                column: None,
                level: if trimmed.starts_with("warning") {
                    "warning".to_string()
                } else {
                    "error".to_string()
                },
                code: None,
                message: trimmed.to_string(),
            });
        }
        if diagnostics.len() >= 10 {
            break;
        }
    }
    if diagnostics.is_empty() {
        diagnostics.push(Diagnostic {
            file: None,
            line: None,
            column: None,
            level: "error".to_string(),
            code: None,
            message: format!("{} failed; see the run log for raw output", run.label),
        });
    }
    diagnostics
}

fn summarize(runs: &[CommandRun], diagnostics: &[Diagnostic]) -> String {
    let mut output = String::new();
    for run in runs {
        if run.success {
            output.push_str(&format!(
                "{}: ok ({:.1}s)\n",
                run.label,
                run.duration_ms as f64 / 1000.0
            ));
        } else if run.timed_out {
            output.push_str(&format!("{}: TIMEOUT\n", run.label));
        } else {
            output.push_str(&format!("{}: FAILED\n", run.label));
        }
    }

    if !diagnostics.is_empty() {
        output.push('\n');
        for diagnostic in diagnostics.iter().take(20) {
            output.push_str(&format_diagnostic(diagnostic));
            output.push('\n');
        }
    }

    if let Some(failures) = extract_test_failures(runs) {
        output.push('\n');
        output.push_str(&failures);
    }

    truncate(&output, 6000)
}

pub fn format_diagnostic(diagnostic: &Diagnostic) -> String {
    let location = match (&diagnostic.file, diagnostic.line, diagnostic.column) {
        (Some(file), Some(line), Some(column)) => format!("{file}:{line}:{column}: "),
        (Some(file), Some(line), None) => format!("{file}:{line}: "),
        (Some(file), None, _) => format!("{file}: "),
        _ => String::new(),
    };
    let code = diagnostic
        .code
        .as_ref()
        .map(|code| format!("[{code}]"))
        .unwrap_or_default();
    format!(
        "{location}{}{code}: {}",
        diagnostic.level, diagnostic.message
    )
}

fn extract_test_failures(runs: &[CommandRun]) -> Option<String> {
    let mut output = String::new();
    for run in runs {
        if run.success || !run.label.contains("test") {
            continue;
        }
        let text = format!("{}\n{}", run.stdout, run.stderr);
        let mut in_failure = false;
        let mut failure_lines = 0usize;
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----") {
                in_failure = true;
                failure_lines = 0;
                output.push_str(trimmed);
                output.push('\n');
                continue;
            }
            if in_failure {
                if trimmed.is_empty() {
                    in_failure = false;
                    output.push('\n');
                    continue;
                }
                if failure_lines < 12 {
                    output.push_str(trimmed);
                    output.push('\n');
                    failure_lines += 1;
                }
                continue;
            }
            if trimmed.contains("panicked at")
                || trimmed.contains("assertion")
                || trimmed.starts_with("test result:")
                || trimmed.starts_with("error[")
                || trimmed.starts_with("error:")
                || trimmed.contains("FAILED")
            {
                output.push_str(trimmed);
                output.push('\n');
            }
            if output.len() > 3500 {
                break;
            }
        }
    }
    if output.trim().is_empty() {
        None
    } else {
        Some(truncate(&output, 3500))
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
    fn parses_cargo_json_diagnostics() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/main.rs","line_start":12,"column_start":5,"is_primary":true}]}}"#;
        let diagnostics = parse_cargo_json(line);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code.as_deref(), Some("E0308"));
        assert_eq!(diagnostics[0].line, Some(12));
        assert!(format_diagnostic(&diagnostics[0]).contains("src/main.rs:12:5: error[E0308]"));
    }

    #[test]
    fn extracts_test_failure_sections() {
        let run = CommandRun {
            label: "cargo test".to_string(),
            success: false,
            timed_out: false,
            stdout: "running 1 test\n---- auth::tests::times_out stdout ----\nthread 'main' panicked at src/auth.rs:10:5:\nassertion failed\ntest result: FAILED. 0 passed; 1 failed\n".to_string(),
            stderr: String::new(),
            duration_ms: 10,
        };
        let text = extract_test_failures(&[run]).unwrap();
        assert!(text.contains("times_out"));
        assert!(text.contains("assertion failed"));
        assert!(text.contains("test result: FAILED"));
    }
}
