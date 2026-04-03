use crate::agent::{extract_assistant_text, run_session};
use crate::config::load_config_file;
use crate::tool_worker;
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "run_agent")]
#[command(about = "Run an AgentFrame session.")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<RunAgentCommand>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg()]
    pub prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum RunAgentCommand {
    #[command(name = "run-tool-worker", hide = true)]
    RunToolWorker {
        #[arg(long)]
        job_file: PathBuf,
    },
}

fn read_prompt(prompt_parts: &[String]) -> Result<String> {
    if !prompt_parts.is_empty() {
        return Ok(prompt_parts.join(" ").trim().to_string());
    }
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;
    Ok(buffer.trim().to_string())
}

pub fn run(argv: impl IntoIterator<Item = String>) -> Result<()> {
    let args = Args::parse_from(argv);
    if let Some(RunAgentCommand::RunToolWorker { job_file }) = args.command {
        return tool_worker::run_job_file(&job_file);
    }
    let config =
        load_config_file(args.config.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--config is required unless running run-tool-worker")
        })?)?;
    let prompt = read_prompt(&args.prompt)?;
    if prompt.is_empty() {
        bail!("provide a prompt argument or pipe prompt text to stdin");
    }
    let messages = run_session(Vec::new(), prompt, config, Vec::new())?;
    let text = extract_assistant_text(&messages);
    println!("{}", text);
    Ok(())
}
