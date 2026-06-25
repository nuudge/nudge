use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::{AgentConfig, Backend, SessionHandle};

mod coding;
mod core;
mod llm;
mod transport;
mod tui;

// (display label, API model id). The TUI's /model picker renders labels;
// the id is what goes on the wire. The list is exactly the models that
// support `thinking: {type: "adaptive"}` — the request shape run_agent
// always sends — so every entry works without per-model request branching.
pub const MODELS: &[(&str, &str)] = &[
    ("Fable 5", "claude-fable-5"),
    ("Mythos 5", "claude-mythos-5"),
    ("Mythos Preview", "claude-mythos-preview"),
    ("Opus 4.8", "claude-opus-4-8"),
    ("Opus 4.7", "claude-opus-4-7"),
    ("Opus 4.6", "claude-opus-4-6"),
    ("Sonnet 4.6", "claude-sonnet-4-6"),
];
const DEFAULT_MODEL: &str = "claude-opus-4-8";
const MAX_TOKENS: u32 = 16384;
const MAX_ITERATIONS: usize = 25;
// Buffer for relay-dial status updates flowing from the handoff task to the TUI.
// A handful of state flips at a time; the TUI drains it in its select loop.
const HANDOFF_STATUS_CAP: usize = 8;
/// Thinking display mode.
#[derive(Clone, ValueEnum)]
enum Thinking {
    /// See Claude's reasoning in the TUI.
    Summarized,
    /// No thinking text (signature only); faster TTFT, same cost.
    Omitted,
}

impl Thinking {
    // The string form the agent + UI configs expect.
    fn as_display(&self) -> String {
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
struct Cli {
    /// Resume a previous session by id or name from ~/.nudge/projects/<cwd>/.
    /// A name (set via /session-rename) is resolved to its session id.
    #[arg(long, value_name = "id-or-name")]
    resume: Option<String>,

    /// List this project's saved sessions (name, id, branch, size, last used) and exit.
    #[arg(long)]
    list: bool,

    /// Thinking display.
    #[arg(long, default_value = "summarized", value_name = "mode")]
    thinking: Thinking,

    /// Print the assembled system prompt + tool schemas and their token cost, then exit.
    #[arg(long)]
    print_prompt: bool,

    /// Host the session headless; clients attach with --connect. Uses $NUDGE_RELAY,
    /// or a local Unix socket when --socket is given.
    #[arg(long, group = "run_mode")]
    daemon: bool,

    /// Attach a TUI to a running --daemon (with --pair-code, or --socket for a local one).
    #[arg(long, group = "run_mode")]
    connect: bool,

    /// Host / attach over a local Unix socket at this path instead of the relay
    /// (debugging: exercise the transport without a relay).
    #[arg(long, value_name = "path")]
    socket: Option<PathBuf>,

    /// (--connect) Attach using a pairing code scanned from the host's QR; self-contained.
    #[arg(
        long,
        value_name = "code",
        requires = "connect",
        conflicts_with = "socket"
    )]
    pair_code: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // --list is a standalone read-only action: print this project's sessions and exit,
    // touching no session and needing no API key.
    if cli.list {
        return print_sessions();
    }

    // --connect owns no session: it just attaches a TUI to a running daemon, so it
    // needs neither an API key nor any of the session setup below.
    if cli.connect {
        return run_connect(cli).await;
    }

    let api_key = env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;

    let (session, initial_messages, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.messages, r.dropped)
        }
    };

    let thinking_display = cli.thinking.as_display();
    let mut ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        model: DEFAULT_MODEL.into(),
        thinking_display: thinking_display.clone(),
        // Filled in the in-process branch below when --relay arms remote pairing.
        pairing_qr: None,
        pairing_code: None,
        // This process hosts the agent loop: it's the owner TUI (can force-reclaim).
        is_owner: true,
    };
    let cfg = AgentConfig {
        model: DEFAULT_MODEL.into(),
        max_tokens: MAX_TOKENS,
        max_iterations: MAX_ITERATIONS,
        thinking_display,
    };
    let provider = llm::AnthropicProvider::new(api_key);

    // Connect to MCP servers declared in the project-local `.mcp.json` before
    // the TUI takes the screen, so connection logs print cleanly to stderr.
    // Missing config = no servers; bad config or failed connects degrade
    // gracefully (logged, skipped) — the agent still runs with built-in tools.
    let mcp_specs = match coding::mcp::load_config(&session.cwd) {
        Ok(specs) => specs,
        Err(e) => {
            eprintln!("[mcp] config error: {e:#}");
            Vec::new()
        }
    };
    let mcp = coding::mcp::McpRegistry::bootstrap(&mcp_specs).await;
    for line in &mcp.connect_log {
        eprintln!("{line}");
    }

    if cli.print_prompt {
        return coding::print_preamble(&cfg, &provider, &session, &mcp).await;
    }

    let backend = coding::CodingBackend::new(session.cwd.clone(), mcp);

    // Pre-translate the resumed transcript to controller events and seed the
    // host's replay buffer with it, so the TUI (and later a remote client)
    // rebuilds history purely from the event stream — no front-end-side JSONL
    // replay. `initial_messages` still seeds the model's conversation in the loop.
    let mut seed = coding::replay_events(&initial_messages, dropped);
    // Prepend the initial session context so every controller has a header on its very
    // first attach, before any turn completes. The loop re-emits SessionInfo on each
    // turn boundary (and on /model) to keep it live; clients only ever render it.
    seed.insert(
        0,
        core::ControllerEvent::SessionInfo {
            model: cfg.model.clone(),
            cwd: session.cwd_display(),
            git_branch: backend.git_branch(),
            session_id: session.id.clone(),
            session_name: session.name.clone(),
        },
    );
    let mut host =
        core::SessionHost::spawn(cfg, provider, backend, session, initial_messages, seed);

    // The relay base URL for phone handoff (and the relay daemon). Optional: a plain
    // local session without it still runs and backgrounds — just no phone handoff.
    let relay = env::var("NUDGE_RELAY").ok();

    // Headless daemon vs local in-process TUI (--connect returned earlier). The
    // daemon dials OUT to the relay by default, or binds a local Unix socket with
    // --socket. Either way it runs until an explicit signal, then shuts down.
    if cli.daemon {
        let broker = host.broker_handle();
        // The long-running daemon. A controller quitting/leaving never ends it, so
        // the only way out is an explicit process signal — we race the two and, on a
        // signal, fall through to a graceful host shutdown below.
        let run = async move {
            if let Some(path) = cli.socket {
                // Local debug daemon: bind a Unix socket; clients attach with
                // --connect --socket <path>.
                let listener = transport::bind_listener(&path)?;
                transport::run_daemon(listener, path, broker).await
            } else {
                // Relay daemon: mint a fresh room + key, print a QR to pair a device,
                // and dial OUT to the relay (the only direction that crosses NAT).
                let base = relay.context(
                    "set NUDGE_RELAY to host over a relay, or pass --socket <path> for a local debug daemon",
                )?;
                let pairing = transport::Pairing::generate(base);
                print_pairing(&pairing)?;
                transport::run_relay_daemon(pairing.dial_url(), pairing.cipher, broker).await
            }
        };
        let daemon_result = tokio::select! {
            r = run => r,
            _ = shutdown_signal() => {
                eprintln!("[daemon] shutdown signal received; stopping");
                Ok(())
            }
        };
        let _ = host.shutdown().await;
        daemon_result
    } else {
        // Local TUI driving the loop in-process. The TUI holds `&host` so it can
        // re-attach after /background; it owns the initial foreground controller.
        // When NUDGE_RELAY is set, /background fires the handoff hook to dial the
        // relay (lazy) so a phone can attach; wiring it here (not in `core`) keeps
        // `core` below the transport layer. The dial reports progress to the TUI over
        // `status_rx`. With no relay configured, /background just pauses in place.
        let handoff_rx = if let Some(base) = relay {
            // Regenerate the pairing each launch (no persistence): the device
            // re-scans after a restart. The QR/code go to the TUI, which surfaces
            // them on /background; the dial-out only opens once backgrounded.
            let pairing = transport::Pairing::generate(base);
            ui_cfg.pairing_qr = Some(pairing.render_qr()?);
            ui_cfg.pairing_code = Some(pairing.encode());
            let dial_url = pairing.dial_url();
            let cipher = pairing.cipher;
            let broker = host.broker_handle();
            let (status_tx, status_rx) = mpsc::channel::<core::HandoffStatus>(HANDOFF_STATUS_CAP);
            // Dedupe re-dials: while one dial is live this is a no-op, so re-entering
            // /background does nothing; once a failed dial clears it, the next
            // /background fires a fresh one (the user's way to retry).
            let dialing = Arc::new(AtomicBool::new(false));
            host.set_handoff_hook(move || {
                if dialing.swap(true, Ordering::SeqCst) {
                    return;
                }
                let dialing = dialing.clone();
                let dial_url = dial_url.clone();
                let cipher = cipher.clone();
                let broker = broker.clone();
                let status_tx = status_tx.clone();
                tokio::spawn(async move {
                    transport::serve_relay_handoff(dial_url, cipher, broker, status_tx).await;
                    dialing.store(false, Ordering::SeqCst);
                });
            });
            Some(status_rx)
        } else {
            None
        };

        let controller = host
            .attach()
            .await
            .expect("initial attach on a fresh session cannot be busy");
        let tui_result = tui::run(&host, ui_cfg, controller, handoff_rx).await;
        // TUI exited → end the session explicitly (loop outlives the front-end).
        let _ = host.shutdown().await;
        tui_result
    }
}

// Attach a TUI to a running daemon (Unix socket or relay). The client owns no loop
// and no session metadata, so the header starts as placeholders and is filled from
// the daemon's `SessionInfo` event — replayed first on attach.
async fn run_connect(cli: Cli) -> Result<()> {
    // Placeholders only: the daemon's SessionInfo event (replayed first on attach)
    // overwrites session id / model / cwd / branch with the daemon's real context.
    let ui_cfg = tui::UiConfig {
        session_id: "(connecting…)".into(),
        session_name: None,
        model: DEFAULT_MODEL.into(),
        thinking_display: cli.thinking.as_display(),
        // A --connect client never hosts a relay, so it shows no pairing QR.
        pairing_qr: None,
        pairing_code: None,
        // A --connect client is a guest: it attaches to a daemon it doesn't host
        // and (per attach_force's default) cannot force-reclaim.
        is_owner: false,
    };
    // Use each client's `connect` rather than the silent `SessionHandle::attach` for
    // this first attach: it runs before the TUI owns the screen, so a connection
    // failure can (and should) surface its cause here. `Err` = transport failure
    // (no daemon / relay unreachable); `None` = up but the session is held elsewhere.
    if let Some(code) = cli.pair_code {
        // A scanned pairing code is self-contained: it carries the relay URL, room
        // id, and E2E key, so it needs no other flags.
        let pairing = transport::Pairing::decode(&code)?;
        let client = transport::RelayClient::new(pairing.dial_url(), pairing.cipher);
        let controller = match client.connect(None).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session is held by another client"),
        };
        tui::run(&client, ui_cfg, controller, None).await
    } else if let Some(path) = cli.socket {
        // Local debug daemon over a Unix socket (--daemon --socket on the other end).
        let client = transport::SocketClient::new(path);
        let controller = match client.connect(None).await? {
            Some(c) => c,
            None => {
                bail!("could not attach: the daemon is busy (another client holds the session)")
            }
        };
        tui::run(&client, ui_cfg, controller, None).await
    } else {
        bail!("--connect needs --pair-code <code> (relay) or --socket <path> (local debug daemon)")
    }
}

// `--list`: print the current project's saved sessions, most-recent-first, so the
// user can pick one to `--resume` by name instead of squinting at uuids. Nameless
// sessions still appear (their id is the only handle). Read-only; no API key needed.
fn print_sessions() -> Result<()> {
    let sessions = coding::list_sessions()?;
    if sessions.is_empty() {
        println!("No saved sessions for this directory.");
        return Ok(());
    }
    println!(
        "{:<28}  {:<36}  {:<16}  {:>8}  LAST USED",
        "NAME", "ID", "BRANCH", "SIZE"
    );
    for s in &sessions {
        let when = chrono::DateTime::<chrono::Local>::from(s.modified).format("%Y-%m-%d %H:%M");
        println!(
            "{:<28}  {:<36}  {:<16}  {:>8}  {}",
            s.name.as_deref().unwrap_or("(unnamed)"),
            s.id,
            s.branch.as_deref().unwrap_or("-"),
            human_size(s.size),
            when,
        );
    }
    println!("\nResume with: nudge --resume <name-or-id>");
    Ok(())
}

// Format a byte count as a short human-readable string (e.g. `1.2K`, `3.4M`) for the
// `--list` SIZE column. Bytes under 1K stay as a plain count so tiny transcripts read
// exactly rather than rounding to `0.0K`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

// Resolve when the process is asked to stop (Ctrl-C or SIGTERM). A daemon is ended
// only by an explicit signal — never by a controller quitting — so this is the one
// thing that breaks the dial/accept loop and lets the session shut down gracefully.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            // Couldn't install the handler: never fire on SIGTERM, leaving Ctrl-C.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// Print the QR + paste-able code that pairs a device. The code is a secret (it
// carries the E2E key), so it goes to the operator's terminal only.
fn print_pairing(pairing: &transport::Pairing) -> Result<()> {
    println!(
        "Scan to pair a device — this code carries the relay address, room id, and E2E key, so keep it secret:\n"
    );
    println!("{}", pairing.render_qr()?);
    println!(
        "Or paste this pairing code on the other device:\n\n{}\n",
        pairing.encode()
    );
    Ok(())
}
