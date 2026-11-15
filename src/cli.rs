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

    /// Thinking display (overrides NUDGE_THINKING; default: summarized).
    #[arg(long, value_name = "mode")]
    pub thinking: Option<Thinking>,

    /// Print the assembled system prompt + tool schemas and their token cost, then exit.
    #[arg(long)]
    pub print_prompt: bool,

    /// Host the session headless; clients attach with --connect. Serves the session's
    /// local socket and $NUDGE_RELAY (only the socket with --local), or a local Unix
    /// socket at a custom path when --socket is given.
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

    /// (--daemon, relay) Also mint a watch-only pairing code: a second QR whose holder
    /// observes the session but cannot drive it, run commands, answer prompts, or quit.
    #[arg(long, requires = "daemon", conflicts_with = "socket")]
    pub watch: bool,

    /// (--daemon, relay) Also mint an agent-peer pairing code: a remote nudge session
    /// dials it with `/connect-peer` to form a mutual, unsupervised peer edge.
    #[arg(long, requires = "daemon", conflicts_with = "socket")]
    pub peer: bool,

    /// (--daemon) Serve only the session's local socket and open no relay legs, even
    /// with $NUDGE_RELAY set. Peers on this machine connect by session id or name.
    #[arg(long, requires = "daemon", conflicts_with_all = ["socket", "watch", "peer"])]
    pub local: bool,

    /// Internal: exit when the last supervisor leaves, or when none attaches within a
    /// grace period. Set on the host `/connect-peer` starts for an ended session.
    #[arg(long, hide = true, requires_all = ["daemon", "resume"])]
    pub needs_supervisor: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parses(args: &[&str]) -> bool {
        Cli::try_parse_from(std::iter::once("nudge").chain(args.iter().copied())).is_ok()
    }

    #[test]
    fn local_and_needs_supervisor_flag_rules() {
        assert!(parses(&[
            "--daemon",
            "--local",
            "--needs-supervisor",
            "--resume",
            "x"
        ]));
        assert!(!parses(&["--local"]));
        for relay_only in ["--watch", "--peer"] {
            assert!(!parses(&["--daemon", "--local", relay_only]));
        }
        assert!(!parses(&["--daemon", "--local", "--socket", "/tmp/s"]));
        assert!(!parses(&["--daemon", "--needs-supervisor"]));
        assert!(!parses(&["--needs-supervisor", "--resume", "x"]));
    }
}
