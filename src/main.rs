use anyhow::Result;
use clap::Parser;

mod cli;
mod coding;
mod config;
mod core;
mod llm;
mod models;
mod run;
mod sessions;
mod spawn;
mod transport;
mod tui;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    config::load_dotenv();

    let cli = Cli::parse();

    // --list and --connect run before `run::host`: they need no API key or session.
    if cli.list {
        return sessions::print_sessions();
    }

    if cli.connect {
        return run::connect::run(cli).await;
    }

    run::host(cli).await
}
