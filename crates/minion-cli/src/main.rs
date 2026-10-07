//! `minion`: a minimal, Unix-native AI agent harness.

mod approval;
mod cli;
mod cron;
mod init;
mod markdown;
mod mcp;
mod mcp_serve;
mod render;
mod repl;
mod run;
mod session;
mod setup;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use minion_core::agent::StopReason;
use minion_core::config::{Config, LogFormat};
use minion_core::error::{Error, Result};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("minion: {err}");
            ExitCode::from(exit_code(&err))
        }
    }
}

async fn execute(cli: Cli) -> Result<ExitCode> {
    let cwd = resolve_cwd(&cli)?;

    // `init` runs before configuration is loaded: its whole purpose is to
    // repair a missing or broken config, so it must not depend on one.
    if let Some(Command::Init(args)) = &cli.command {
        init_logging("info", LogFormat::Text, cli.verbose);
        return init::run(&cli, args.clone()).await;
    }

    let mut config = Config::load(cli.config.as_deref(), &cwd)?;
    if let Some(model) = &cli.model {
        config.provider.model = model.clone();
    }
    if let Some(base_url) = &cli.base_url {
        config.provider.base_url = base_url.clone();
    }
    if let Some(name) = &cli.api_key_env {
        config.provider.api_key_env = name.clone();
    }
    init_logging(&config.logging.level, config.logging.format, cli.verbose);

    match &cli.command {
        Some(Command::Init(_)) => unreachable!("handled above"),
        Some(Command::Session(args)) => session::run(&cli, &config, args.clone()).await,
        Some(Command::Cron(args)) => cron::run(&cli, &config, args.clone()).await,
        Some(Command::Mcp(args)) => mcp::run(&cli, &config, &cwd, args.clone()).await,
        Some(Command::Run { prompt }) => {
            let prompt = resolve_prompt(prompt)?;
            let stop = run::one_shot(&cli, &config, &cwd, prompt).await?;
            Ok(if stop == StopReason::Completed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        None => {
            repl::interactive(&cli, &config, &cwd, None).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn resolve_cwd(cli: &Cli) -> Result<PathBuf> {
    let cwd = match cli.cwd.clone() {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    Ok(cwd.canonicalize().unwrap_or(cwd))
}

/// `-` means "read the prompt from stdin", which makes minion pipeable.
fn resolve_prompt(prompt: &str) -> Result<String> {
    if prompt != "-" {
        return Ok(prompt.to_string());
    }
    use std::io::Read;
    let mut buffer = String::new();
    std::io::stdin().read_to_string(&mut buffer)?;
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return Err(Error::Config("no prompt was provided on stdin".to_string()));
    }
    Ok(trimmed.to_string())
}

/// Logs always go to stderr so stdout stays machine-readable.
fn init_logging(level: &str, format: LogFormat, verbose: u8) {
    let mut level = match verbose {
        0 => level.to_string(),
        1 => "debug".to_string(),
        _ => "trace".to_string(),
    };
    // `rmcp` announces every service, task cancellation and shutdown at INFO.
    // That is protocol chatter on a channel the REPL uses for tool activity, so
    // it is quieted at the default verbosity and left alone once `-v` asks for
    // the detail.
    if verbose == 0 && !level.contains("rmcp") {
        level.push_str(",rmcp=warn");
    }
    let filter = tracing_subscriber::EnvFilter::try_new(&level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);

    if format == LogFormat::Json {
        let _ = builder.json().try_init();
    } else {
        let _ = builder.try_init();
    }
}

/// Exit codes documented in the SDD.
fn exit_code(err: &Error) -> u8 {
    match err {
        Error::Config(_) => 2,
        Error::Auth(_)
        | Error::Provider(_)
        | Error::RateLimit { .. }
        | Error::BadRequest { .. } => 3,
        Error::Denied(_) => 4,
        _ => 5,
    }
}
