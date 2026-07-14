use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// Thinking display mode.
#[derive(Clone, ValueEnum)]
pub enum Thinking {
    /// See Claude's reasoning in the TUI.
    Summarized,
    /// No thinking text (signature only); faster TTFT, same cost.
    Omitted,
}

impl Thinking {
    // The string form the agent + UI configs expect.
    pub fn as_display(&self) -> String {
        match self {
            Thinking::Summarized => "summarized",
            Thinking::Omitted => "omitted",
        }
        .to_string()
    }
}

/// A Claude-Code-style coding agent for your terminal.
///
/// With no mode flag it runs an interactive TUI in-process; /background then dials
/// the relay from $NUDGE_RELAY and shows a QR for a phone to attach. `--daemon` hosts
/// the session headless over that relay (a `--connect --pair-code` client attaches).
/// `--daemon --socket` / `--connect --socket` host / attach over a local Unix socket
/// instead — kept for debugging the transport without a relay.
#[derive(Parser)]
#[command(name = "nudge", version)]
pub struct Cli {
    /// Resume a previous session by id or name from ~/.nudge/projects/<cwd>/.
    /// A name (set via /session-rename) is resolved to its session id.
    #[arg(long, value_name = "id-or-name")]
    pub resume: Option<String>,

    /// List this project's saved sessions (name, id, branch, size, last used) and exit.
    #[arg(long)]
    pub list: bool,

    /// Thinking display.
    #[arg(long, default_value = "summarized", value_name = "mode")]
    pub thinking: Thinking,

    /// Print the assembled system prompt + tool schemas and their token cost, then exit.
    #[arg(long)]
    pub print_prompt: bool,

    /// Host the session headless; clients attach with --connect. Uses $NUDGE_RELAY,
    /// or a local Unix socket when --socket is given.
    #[arg(long, group = "run_mode")]
    pub daemon: bool,

    /// Attach a TUI to a running --daemon (with --pair-code, or --socket for a local one).
    #[arg(long, group = "run_mode")]
    pub connect: bool,

    /// Host / attach over a local Unix socket at this path instead of the relay
    /// (debugging: exercise the transport without a relay).
    #[arg(long, value_name = "path")]
    pub socket: Option<PathBuf>,

    /// (--connect) Attach using a pairing code scanned from the host's QR; self-contained.
    #[arg(
        long,
        value_name = "code",
        requires = "connect",
        conflicts_with = "socket"
    )]
    pub pair_code: Option<String>,
}
