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

    // --list is a standalone read-only action: print this project's sessions and exit,
    // touching no session and needing no API key.
    if cli.list {
        return sessions::print_sessions();
    }

    // --connect owns no session: it just attaches a TUI to a running daemon, so it
    // needs neither an API key nor any of the session setup in `run::host`.
    if cli.connect {
        return run::connect::run(cli).await;
    }

    run::host(cli).await
}
