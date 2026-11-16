use anyhow::Result;
use clap::Parser;

mod cli;
mod coding;
mod config;
mod core;
mod llm;
mod logging;
mod models;
mod run;
mod sessions;
mod transport;
mod tui;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    config::load_dotenv();

    let cli = Cli::parse();

    // These run before `run::host`: they need no API key or session.
    if cli.list {
        return sessions::print_sessions();
    }

    if cli.prune_logs {
        return sessions::prune_logs(cli.older_than.unwrap_or(logging::DEFAULT_PRUNE_AGE));
    }

    if let Some(reference) = &cli.log {
        return sessions::print_log(reference);
    }

    if let Some(target) = &cli.attach {
        return run::attach::run(target, &cli).await;
    }

    // A no-op if the host failed before it installed the logger.
    let result = run::host(cli).await;
    match &result {
        Ok(()) => tracing::info!("session host stopped"),
        Err(e) => tracing::error!(error = %format!("{e:#}"), "session host failed"),
    }
    result
}
