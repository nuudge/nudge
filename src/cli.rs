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
/// the session headless on its local session socket and over that relay, or only on the
/// socket with `--local`. `--attach` opens a TUI on a running session, local or remote.
/// `--connect` makes another session a peer of this one at startup.
#[derive(Parser)]
#[command(name = "nudge", version)]
pub struct Cli {
    /// Resume a previous session by id, short id (its first 8 characters) or name. A name
    /// (set via /session-rename) matches only this directory's sessions.
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

    /// Host the session headless on its local socket and over $NUDGE_RELAY, which must
    /// be set unless --local is given. Runs until stopped; clients attach with --attach.
    #[arg(long, group = "run_mode")]
    pub daemon: bool,

    /// Attach a TUI to a running session: a session id or short id (any project) or name
    /// (this directory) on this machine, or a pairing code from a remote host's QR.
    #[arg(long, value_name = "id|name|pairing-code", group = "run_mode")]
    pub attach: Option<String>,

    /// At startup, make another session a peer of this one, exactly as `/connect-peer`
    /// does: an ended local session is resumed in the background first, and a failure
    /// shows up as a notice while the session starts anyway.
    #[arg(
        long,
        value_name = "id|name|pairing-code",
        conflicts_with_all = ["attach", "needs_answerer"]
    )]
    pub connect: Option<String>,

    /// (--daemon) Also mint a watch-only pairing code: a second QR whose holder observes
    /// the session but cannot drive it, run commands, answer prompts, or quit.
    #[arg(long, requires = "daemon")]
    pub watch: bool,

    /// (--daemon) Also mint an agent-peer pairing code: a remote nudge session dials it
    /// with `/connect-peer` to form a mutual, unsupervised peer edge.
    #[arg(long, requires = "daemon")]
    pub peer: bool,

    /// (--daemon) Serve only the session's local socket and open no relay legs, even
    /// with $NUDGE_RELAY set. Peers on this machine connect by session id or name.
    #[arg(long, requires = "daemon", conflicts_with_all = ["watch", "peer"])]
    pub local: bool,

    /// Internal: exit when no one is left who can answer its permission prompts (a human
    /// TUI or a supervising agent), or when none attaches within a grace period. Set on
    /// the host `/connect-peer` starts for an ended session.
    #[arg(long, hide = true, requires_all = ["daemon", "resume"])]
    pub needs_answerer: bool,
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
        assert!(
            parse(&[
                "--daemon",
                "--resume",
                "me",
                "--needs-answerer",
                "--connect",
                "w"
            ])
            .is_err()
        );
    }

    #[test]
    fn local_and_needs_answerer_flag_rules() {
        assert!(parse(&["--daemon", "--local", "--needs-answerer", "--resume", "x"]).is_ok());
        assert!(parse(&["--local"]).is_err());
        for relay_only in ["--watch", "--peer"] {
            assert!(parse(&["--daemon", "--local", relay_only]).is_err());
        }
        assert!(parse(&["--daemon", "--needs-answerer"]).is_err());
        assert!(parse(&["--needs-answerer", "--resume", "x"]).is_err());
    }

    #[test]
    fn the_old_attach_flags_are_gone() {
        assert!(parse(&["--pair-code", "nudge:x"]).is_err());
        assert!(parse(&["--daemon", "--socket", "/tmp/s"]).is_err());
    }
}
