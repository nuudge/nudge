use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::env;
use std::path::PathBuf;

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
/// With no mode flag it runs an interactive TUI in-process. `--daemon` hosts the
/// session headless (a `--connect` client drives it), over a Unix socket by
/// default or a relay with `--relay`.
#[derive(Parser)]
#[command(name = "nudge", version)]
struct Cli {
    /// Resume a previous session from ~/.nudge/projects/<cwd>/<id>.jsonl.
    #[arg(long, value_name = "id")]
    resume: Option<String>,

    /// Thinking display.
    #[arg(long, default_value = "summarized", value_name = "mode")]
    thinking: Thinking,

    /// Print the assembled system prompt + tool schemas and their token cost, then exit.
    #[arg(long)]
    print_prompt: bool,

    /// Host the session headless behind a socket; clients attach with --connect.
    #[arg(long, group = "run_mode")]
    daemon: bool,

    /// Attach a TUI to a running --daemon instead of starting a local session.
    #[arg(long, group = "run_mode")]
    connect: bool,

    /// Socket path for --daemon / --connect (default: ~/.nudge/daemon.sock).
    #[arg(long, value_name = "path", conflicts_with = "relay")]
    socket: Option<PathBuf>,

    /// Host (--daemon) or attach (--connect) over a WebSocket relay; E2E encrypted. Pass --pair or --key.
    #[arg(long, value_name = "ws-url")]
    relay: Option<String>,

    /// Pre-shared E2E key file for the relay path; load the same key on both ends.
    #[arg(long, value_name = "path")]
    key: Option<PathBuf>,

    /// Write a fresh relay key to --key <path> and exit.
    #[arg(long, requires = "key", conflicts_with_all = ["daemon", "connect", "relay", "socket"])]
    gen_key: bool,

    /// (--daemon --relay <base-url>) Mint a fresh room + E2E key and show a QR to pair a device.
    #[arg(long, requires = "relay", conflicts_with = "connect")]
    pair: bool,

    /// (--connect) Attach using a pairing code scanned from the daemon's QR; self-contained.
    #[arg(long, value_name = "code", requires = "connect", conflicts_with_all = ["relay", "socket", "key"])]
    pair_code: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // --gen-key is a standalone action: write a fresh relay key and exit, touching
    // no session and needing no API key. (clap guarantees --key is present.) Pair
    // both devices off this one file (8.3 replaces it with QR-derived keys).
    if cli.gen_key {
        let path = cli.key.expect("clap enforces --gen-key requires --key");
        transport::Cipher::generate().save(&path)?;
        println!(
            "Wrote a new relay key to {} — keep it secret; load the same file on both ends.",
            path.display()
        );
        return Ok(());
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
    let ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        model: DEFAULT_MODEL.into(),
        thinking_display: thinking_display.clone(),
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
        },
    );
    let mut host = core::SessionHost::spawn(cfg, provider, backend, session, initial_messages, seed);

    // Headless daemon vs local in-process TUI (--connect returned earlier). Over the
    // relay the daemon dials OUT (--relay); otherwise it binds a local Unix socket.
    // Either way it runs until a controller quits the session, then shuts down.
    if cli.daemon {
        let broker = host.broker_handle();
        // The long-running daemon. A controller quitting/leaving never ends it, so
        // the only way out is an explicit process signal — we race the two and, on a
        // signal, fall through to a graceful host shutdown below.
        let run = async move {
            match cli.relay {
                Some(base) => {
                    // --pair mints a fresh room + key and prints a QR for a device to
                    // scan; otherwise the relay path is keyed by an explicit --key file.
                    let (url, cipher) = if cli.pair {
                        let pairing = transport::Pairing::generate(base);
                        print_pairing(&pairing)?;
                        (pairing.dial_url(), pairing.cipher)
                    } else {
                        (base, require_relay_key(cli.key)?)
                    };
                    transport::run_relay_daemon(url, cipher, broker).await
                }
                None => {
                    let path = resolve_socket_path(cli.socket)?;
                    let listener = transport::bind_listener(&path)?;
                    transport::run_daemon(listener, path, broker).await
                }
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
        // Lazy handoff: the first /background fires this hook, which binds the
        // socket once and spawns the accept loop so another client can take over
        // while the local TUI is backgrounded. Wiring the socket here (not in
        // `core`) keeps `core` below the transport layer. Bound once and kept for
        // the process lifetime; the file is removed on clean exit.
        let path = resolve_socket_path(cli.socket)?;
        let handoff = host.broker_handle();
        let handoff_path = path.clone();
        host.set_handoff_hook(move || match transport::bind_listener(&handoff_path) {
            Ok(listener) => {
                handoff.notice(format!(
                    "Session handoff enabled — attach from another client with: nudge --connect --socket {}",
                    handoff_path.display()
                ));
                tokio::spawn(transport::serve_handoff(listener, handoff.clone()));
            }
            Err(e) => handoff.notice(format!("Handoff unavailable: {e:#}")),
        });

        let controller = host
            .attach()
            .await
            .expect("initial attach on a fresh session cannot be busy");
        let tui_result = tui::run(&host, ui_cfg, controller).await;
        // TUI exited → end the session explicitly (loop outlives the front-end).
        let _ = host.shutdown().await;
        // Remove the handoff socket if it was ever bound (harmless if not).
        let _ = std::fs::remove_file(&path);
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
        model: DEFAULT_MODEL.into(),
        thinking_display: cli.thinking.as_display(),
    };
    // Use each client's `connect` rather than the silent `SessionHandle::attach` for
    // this first attach: it runs before the TUI owns the screen, so a connection
    // failure can (and should) surface its cause here. `Err` = transport failure
    // (no daemon / relay unreachable); `None` = up but the session is held elsewhere.
    if let Some(code) = cli.pair_code {
        // A scanned pairing code is self-contained: it carries the relay URL, room
        // id, and E2E key, so it needs neither --relay nor --key.
        let pairing = transport::Pairing::decode(&code)?;
        let client = transport::RelayClient::new(pairing.dial_url(), pairing.cipher);
        let controller = match client.connect(None).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session is held by another client"),
        };
        tui::run(&client, ui_cfg, controller).await
    } else if let Some(url) = cli.relay {
        let cipher = require_relay_key(cli.key)?;
        let client = transport::RelayClient::new(url, cipher);
        let controller = match client.connect(None).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session is held by another client"),
        };
        tui::run(&client, ui_cfg, controller).await
    } else {
        let path = resolve_socket_path(cli.socket)?;
        let client = transport::SocketClient::new(path);
        let controller = match client.connect(None).await? {
            Some(c) => c,
            None => bail!("could not attach: the daemon is busy (another client holds the session)"),
        };
        tui::run(&client, ui_cfg, controller).await
    }
}

// Resolve the daemon socket path: an explicit --socket wins, else a fixed default
// under ~/.nudge/ (whose parent is created if absent). One default path
// means one daemon per user; run multiple with distinct --socket values.
fn resolve_socket_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let home = env::var("HOME").context("HOME env var not set")?;
    let dir = PathBuf::from(home).join(".nudge");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join("daemon.sock"))
}

// Load the relay's pre-shared E2E key. Used on the --relay path when --pair is not
// given: the relayed path is always encrypted (only the trusted local socket runs
// plaintext), so a missing key is a hard error with a how-to-fix hint.
fn require_relay_key(key_path: Option<PathBuf>) -> Result<transport::Cipher> {
    let path = key_path.context(
        "--relay needs either --pair (generate a code) or --key <path> (create one with: nudge --gen-key --key <path>)",
    )?;
    transport::Cipher::load(&path)
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
    println!("Or paste this pairing code on the other device:\n\n{}\n", pairing.encode());
    Ok(())
}

