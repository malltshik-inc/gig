mod agent;
mod detect;
mod git;
mod lang;
mod model;
mod pipeline;
mod preflight;
mod repomap;
mod runlog;
mod snapshot;
mod ui;
mod verify;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "gig",
    version,
    about = "A local patch-and-commit agent for Rust projects"
)]
pub struct Cli {
    /// Task description. Reads from stdin when omitted.
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    pub prompt: Vec<String>,

    /// Repository directory (defaults to the current directory).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,

    /// Ollama model to use.
    #[arg(long, default_value = "qwen2.5-coder:7b-instruct")]
    pub model: String,

    /// Context window passed to Ollama.
    #[arg(long, default_value_t = 16384)]
    pub num_ctx: u32,

    /// Maximum edit/verify attempts.
    #[arg(long, default_value_t = 3)]
    pub max_attempts: u32,

    /// Maximum files the planner may touch.
    #[arg(long, default_value_t = 6)]
    pub max_files: usize,

    /// Skip all confirmation prompts.
    #[arg(long)]
    pub yes: bool,

    /// Print the plan and stop.
    #[arg(long)]
    pub plan_only: bool,

    /// Run the full pipeline but never commit, restoring the tree afterwards.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the baseline verification run.
    #[arg(long)]
    pub no_baseline: bool,

    /// Print raw model responses.
    #[arg(long)]
    pub verbose: bool,
}

fn read_prompt(args: &[String]) -> Result<String> {
    if !args.is_empty() {
        return Ok(args.join(" "));
    }
    if std::io::stdin().is_terminal() {
        bail!("no prompt given; pass a task string or pipe one on stdin");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read prompt from stdin")?;
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        bail!("empty prompt on stdin");
    }
    Ok(trimmed)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let prompt = match read_prompt(&cli.prompt) {
        Ok(prompt) => prompt,
        Err(err) => {
            ui::error(&format!("{err:#}"));
            std::process::exit(1);
        }
    };

    match pipeline::run(cli, prompt).await {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            ui::error(&format!("{err:#}"));
            std::process::exit(1);
        }
    }
}
