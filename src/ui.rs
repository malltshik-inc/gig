use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};

use crate::model::Plan;
use crate::verify::VerifyResult;

fn is_tty() -> bool {
    io::stdout().is_terminal()
}

fn paint(text: &str, code: &str) -> String {
    if is_tty() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn banner() {
    println!(
        "{} {}",
        paint("gig", "1;36"),
        paint(concat!("v", env!("CARGO_PKG_VERSION")), "2")
    );
    println!("{}", paint("local patch-and-commit agent", "2"));
}

pub fn stage(title: &str) {
    println!("\n{} {}", paint("›", "1;36"), paint(title, "1"));
}

pub fn info(message: &str) {
    println!("  {message}");
}

pub fn detail(message: &str) {
    for line in message.lines() {
        println!("    {line}");
    }
}

pub fn ok(message: &str) {
    println!("  {} {}", paint("✓", "32"), message);
}

pub fn warn(message: &str) {
    println!("  {} {}", paint("!", "33"), message);
}

pub fn error(message: &str) {
    eprintln!("{} {}", paint("error:", "1;31"), message);
}

pub fn confirm(question: &str, default_yes: bool, auto_yes: bool) -> Result<bool> {
    if auto_yes {
        return Ok(true);
    }
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("  {} {hint} ", paint(question, "1"));
    io::stdout().flush().context("failed to flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read confirmation")?;
    let answer = line.trim().to_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

pub fn ask(question: &str) -> Result<String> {
    print!("  {} ", paint(question, "1"));
    io::stdout().flush().context("failed to flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read input")?;
    Ok(line.trim().to_string())
}

pub fn print_plan(plan: &Plan) {
    if !plan.plan.trim().is_empty() {
        for line in plan.plan.trim().lines() {
            println!("  {line}");
        }
        println!();
    }
    for (index, file) in plan.files.iter().enumerate() {
        let action = if file.create { "create" } else { "edit" };
        println!(
            "  {}. {} {}",
            index + 1,
            paint(action, if file.create { "33" } else { "36" }),
            paint(&file.path, "1")
        );
        if !file.intent.trim().is_empty() && file.intent.trim() != action {
            println!("     {}", file.intent.trim());
        }
        if !file.symbols.is_empty() {
            println!("     symbols: {}", file.symbols.join(", "));
        }
    }
}

pub fn print_verify(result: &VerifyResult) {
    for line in result.summary.lines() {
        if line.starts_with("cargo") && line.contains("FAILED") {
            warn(line);
        } else if line.starts_with("cargo") && line.contains("ok") {
            ok(line);
        } else if !line.trim().is_empty() {
            detail(line);
        }
    }
}

pub fn print_diff(stat: &str, diff: &str) {
    if !stat.trim().is_empty() {
        for line in stat.lines() {
            println!("  {line}");
        }
        println!();
    }
    let max = 20_000;
    let body = if diff.len() > max {
        format!(
            "{}\n… diff truncated at {} bytes; full diff is in the run log",
            &diff[..safe_boundary(diff, max)],
            max
        )
    } else {
        diff.to_string()
    };
    for line in body.lines() {
        let colored = if line.starts_with('+') && !line.starts_with("+++") {
            paint(line, "32")
        } else if line.starts_with('-') && !line.starts_with("---") {
            paint(line, "31")
        } else if line.starts_with("@@") {
            paint(line, "36")
        } else if line.starts_with("diff ") || line.starts_with("index ") {
            paint(line, "2")
        } else {
            line.to_string()
        };
        println!("  {colored}");
    }
}

fn safe_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}
