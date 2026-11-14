use clap::{Parser, ValueEnum};

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
/// the session headless: always on its local session socket, and over that relay when
/// $NUDGE_RELAY is set. `--attach` opens a TUI on a running session, local or remote.
/// `--connect` makes another session a peer of this one at startup.
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

    /// Host the session headless on its local socket, and over $NUDGE_RELAY when set.
    /// Runs until stopped; clients attach with --attach.
    #[arg(long, group = "run_mode")]
    pub daemon: bool,

    /// Attach a TUI to a running session: a session id (any project) or name (this
    /// directory) on this machine, or a pairing code from a remote host's QR.
    #[arg(long, value_name = "id|name|pairing-code", group = "run_mode")]
    pub attach: Option<String>,

    /// At startup, make another session a peer of this one, exactly as `/connect-peer`
    /// does: an ended local session is resumed in the background first, and a failure
    /// shows up as a notice while the session starts anyway.
    #[arg(
        long,
        value_name = "id|name|pairing-code",
        conflicts_with_all = ["attach", "peer_host"]
    )]
    pub connect: Option<String>,

    /// (--daemon, needs $NUDGE_RELAY) Also mint a watch-only pairing code: a second QR
    /// whose holder observes the session but cannot drive it, run commands, answer
    /// prompts, or quit.
    #[arg(long, requires = "daemon")]
    pub watch: bool,

    /// (--daemon, needs $NUDGE_RELAY) Also mint an agent-peer pairing code: a remote
    /// nudge session dials it with `/connect-peer` to form a mutual, unsupervised peer
    /// edge.
    #[arg(long, requires = "daemon")]
    pub peer: bool,

    /// Internal: a headless host started by `/connect-peer` for an ended session. It
    /// exits when its last supervisor leaves.
    #[arg(long, hide = true, requires = "resume", group = "run_mode")]
    pub peer_host: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("nudge").chain(args.iter().copied()))
    }

    #[test]
    fn attach_takes_one_target() {
        assert_eq!(
            parse(&["--attach", "worker"]).unwrap().attach.as_deref(),
            Some("worker")
        );
        assert!(parse(&["--attach"]).is_err());
        assert!(parse(&["--attach", "a", "--daemon"]).is_err());
    }

    #[test]
    fn connect_modifies_a_fresh_resumed_or_daemon_run() {
        assert_eq!(
            parse(&["--connect", "worker"]).unwrap().connect.as_deref(),
            Some("worker")
        );
        let cli = parse(&["--resume", "me", "--connect", "worker"]).unwrap();
        assert_eq!(cli.resume.as_deref(), Some("me"));
        assert!(parse(&["--daemon", "--connect", "worker"]).unwrap().daemon);
        assert!(parse(&["--connect"]).is_err());
        assert!(parse(&["--connect", "w", "--attach", "x"]).is_err());
        assert!(parse(&["--connect", "w", "--resume", "me", "--peer-host"]).is_err());
    }

    #[test]
    fn the_old_attach_flags_are_gone() {
        assert!(parse(&["--pair-code", "nudge:x"]).is_err());
        assert!(parse(&["--daemon", "--socket", "/tmp/s"]).is_err());
    }
}
