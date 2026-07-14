use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

use crate::core::{AgentConfig, Backend, ClientIdentity, ClientKind, SessionHandle};

mod coding;
mod config;
mod core;
mod llm;
mod transport;
mod tui;

use crate::config::Config;

// (display label, API model id). The TUI's /model picker renders labels;
// the id is what goes on the wire. Used as-is by guest/--connect clients
// (no local API key to fetch with) and as the fallback when the owning
// process's `list_models` call fails (offline, bad key, etc).
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
    config::load_dotenv();

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

    let config = Config::from_env()?;
    // Cloned before the provider takes ownership below, so the --spawn-demo child can
    // build its own provider with the same key.
    let api_key = config.anthropic_api_key.clone();

    let (session, initial_messages, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.messages, r.dropped)
        }
    };

    let thinking_display = cli.thinking.as_display();
    let who = local_identity();
    let mut ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        model: DEFAULT_MODEL.into(),
        thinking_display: thinking_display.clone(),
        // Filled in the in-process branch below when --relay arms remote pairing.
        pairing_qr: None,
        pairing_code: None,
        // This process hosts the agent loop: it's the owner TUI (cosmetic badge only).
        is_owner: true,
        user_name: who.name.clone(),
        models: owned_models(MODELS),
    };
    let cfg = AgentConfig {
        model: DEFAULT_MODEL.into(),
        max_tokens: MAX_TOKENS,
        max_iterations: MAX_ITERATIONS,
        thinking_display,
    };
    let provider = llm::AnthropicProvider::new(config.anthropic_api_key);

    ui_cfg.models = resolve_models(&provider, MODELS).await;

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

    // Discover Skills under ~/.nudge/skills/ (personal) and <cwd>/.nudge/skills/
    // (project) before the TUI takes the screen, so discovery — including a
    // malformed SKILL.md being skipped — prints cleanly to stderr.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let skills = coding::skills::SkillRegistry::discover(&session.cwd, home.as_deref());
    for line in &skills.discovery_log {
        eprintln!("{line}");
    }

    if cli.print_prompt {
        return coding::print_preamble(&cfg, &provider, &session, &mcp, &skills).await;
    }

    let backend = coding::CodingBackend::new(session.cwd.clone(), mcp, skills);

    // Pre-translate the resumed transcript to controller events and seed the
    // host's replay buffer with it, so the TUI (and later a remote client)
    // rebuilds history purely from the event stream — no front-end-side JSONL
    // replay. `initial_messages` still seeds the model's conversation in the loop.
    let mut seed = coding::replay_events(&initial_messages, dropped, &who.name);
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
    // The executor behind the model-facing Spawn tool: this session may create
    // subagents (which themselves may not — the factory builds children without one).
    let factory = peer_factory(api_key.clone(), session.id.clone());
    let mut host = core::SessionHost::spawn(
        cfg,
        provider,
        backend,
        session,
        initial_messages,
        seed,
        core::PeerWiring {
            factory: Some(factory),
            initial_peers: Default::default(),
        },
    );

    // The relay base URL for phone handoff (and the relay daemon). Optional: a plain
    // local session without it still runs and backgrounds — just no phone handoff.
    let relay = config.relay;

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
                transport::run_relay_daemon(pairing.host_dial_url(), pairing.cipher, broker).await
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
            let dial_url = pairing.host_dial_url();
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
            .attach(who.clone())
            .await
            .expect("initial attach on a fresh session cannot be busy");
        let tui_result = tui::run(&host, ui_cfg, who, controller, handoff_rx).await;
        // TUI exited → end the session explicitly (loop outlives the front-end).
        let _ = host.shutdown().await;
        tui_result
    }
}

// Identity a spawned agent announces at attach — the first real use of
// `ClientKind::Agent`. `session_id`/`task` give a peer legible provenance in the other
// side's transcript and Notices.
fn agent_identity(
    name: String,
    session_id: Option<String>,
    task: Option<String>,
) -> ClientIdentity {
    ClientIdentity {
        kind: ClientKind::Agent,
        name,
        session_id,
        task,
    }
}

// Build the executor behind the model-facing Spawn tool (`core::PeerFactory`). Each
// call stands up a built-ins-only child agent in the current directory, mutually
// attaches it to the spawner (whose broker handle the loop passes in), kicks it with
// the task, and returns the registration — including the child's `SessionHost`, which
// the spawner's `Peer` then owns as the keep-alive (reaping the peer ends the child).
// The child gets NO factory of its own, so there is no recursive spawning yet.
fn peer_factory(api_key: String, parent_session_id: String) -> core::PeerFactory {
    Box::new(move |task: String, parent: core::BrokerHandle| {
        let api_key = api_key.clone();
        let parent_session_id = parent_session_id.clone();
        Box::pin(async move {
            let session = coding::open_new()?;
            let child_id = session.id.clone();
            let cwd = session.cwd.clone();

            let short: String = child_id.chars().take(8).collect();
            let parent_short: String = parent_session_id.chars().take(8).collect();
            let child_who =
                agent_identity(format!("child-{short}"), Some(child_id), Some(task.clone()));
            let parent_who = agent_identity(
                format!("parent-{parent_short}"),
                Some(parent_session_id),
                Some(task.clone()),
            );

            // Built-ins only: no .mcp.json servers. Skills are still discovered
            // locally (cheap). `into_subagent` installs the role contract (report back
            // to the spawner via MessagePeer) and drops CLAUDE.md — a subagent runs
            // under its spawner's contract, not the repo's human conventions.
            let mcp = coding::mcp::McpRegistry::bootstrap(&[]).await;
            let skills = coding::skills::SkillRegistry::discover(&cwd, None);
            let backend = coding::CodingBackend::new(cwd, mcp, skills)
                .into_subagent(coding::prompt::subagent_role(&parent_who.name));
            let cfg = AgentConfig {
                model: DEFAULT_MODEL.into(),
                max_tokens: MAX_TOKENS,
                max_iterations: MAX_ITERATIONS,
                thinking_display: "omitted".into(),
            };
            let provider = llm::AnthropicProvider::new(api_key);

            // Two edges, and on each one `attach(who)` announces the ATTACHER — the
            // broker stamps that identity on everything sent through the controller,
            // so getting a side wrong mislabels every message on that edge (the
            // child would see its own name on the parent's messages). The
            // registrations are the mirror: each PeerSet records whom that side
            // HOLDS.
            //
            // Return edge (child → parent), seeded into the child's PeerSet at spawn
            // (not raced through a runtime registration), so the child can address
            // its spawner via MessagePeer from its very first turn. The child is the
            // attacher here, so it announces child_who.
            let parent_ctrl = parent
                .attach(child_who.clone())
                .await
                .context("child could not attach back to its spawner")?;
            let mut child_peers = core::peer::PeerSet::default();
            child_peers.register(core::PeerRegistration::new(parent_ctrl, parent_who.clone()));

            let child = core::SessionHost::spawn(
                cfg,
                provider,
                backend,
                session,
                Vec::new(),
                Vec::new(),
                core::PeerWiring {
                    // No factory: a child may not spawn its own subagents yet.
                    factory: None,
                    initial_peers: child_peers,
                },
            );

            // Spawner's edge (parent → child): the parent is the attacher, so it
            // announces parent_who; then it kicks the child with the task. Events
            // buffer until the spawner's loop drains them.
            let child_ctrl = child
                .attach(parent_who.clone())
                .await
                .context("could not attach to the spawned child")?;
            let _ = child_ctrl
                .ui_tx
                .send(core::UiEvent::UserMessage { text: task })
                .await;

            Ok(core::PeerRegistration {
                controller: child_ctrl,
                who: child_who,
                host: Some(child),
            })
        })
    })
}

// The local user's identity, announced at attach. `$USER` if set, else a neutral
// default — a `--name` override can come later.
fn local_identity() -> ClientIdentity {
    let name = std::env::var("USER").unwrap_or_else(|_| "human".into());
    ClientIdentity::human(name)
}

// Attach a TUI to a running daemon (Unix socket or relay). The client owns no loop
// and no session metadata, so the header starts as placeholders and is filled from
// the daemon's `SessionInfo` event — replayed first on attach.
async fn run_connect(cli: Cli) -> Result<()> {
    // Placeholders only: the daemon's SessionInfo event (replayed first on attach)
    // overwrites session id / model / cwd / branch with the daemon's real context.
    let who = local_identity();
    let ui_cfg = tui::UiConfig {
        session_id: "(connecting…)".into(),
        session_name: None,
        model: DEFAULT_MODEL.into(),
        thinking_display: cli.thinking.as_display(),
        // A --connect client never hosts a relay, so it shows no pairing QR.
        pairing_qr: None,
        pairing_code: None,
        // A --connect client is a guest: it attaches to a daemon it doesn't host
        // (cosmetic badge only — clients coexist, none reclaims).
        is_owner: false,
        user_name: who.name.clone(),
        // A guest has no local API key to fetch a fresh list with; the daemon
        // (the owner process) is the one that talks to the provider.
        models: owned_models(MODELS),
    };
    // Use each client's `connect` rather than the silent `SessionHandle::attach` for
    // this first attach: it runs before the TUI owns the screen, so a connection
    // failure can (and should) surface its cause here. `Err` = transport failure
    // (no daemon / relay unreachable); `None` = reachable but the session is gone
    // (the daemon's broker has shut down).
    if let Some(code) = cli.pair_code {
        // A scanned pairing code is self-contained: it carries the relay URL, room
        // id, and E2E key, so it needs no other flags.
        let pairing = transport::Pairing::decode(&code)?;
        let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the relay session has ended"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
    } else if let Some(path) = cli.socket {
        // Local debug daemon over a Unix socket (--daemon --socket on the other end).
        let client = transport::SocketClient::new(path);
        let controller = match client.connect(None, who.clone()).await? {
            Some(c) => c,
            None => bail!("could not attach: the daemon has no live session"),
        };
        tui::run(&client, ui_cfg, who, controller, None).await
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

fn owned_models(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(l, i)| (l.to_string(), i.to_string()))
        .collect()
}

// Refreshes the /model picker from the provider so newly released models show
// up without a code change; keeps the static `fallback` on any fetch failure.
async fn resolve_models(
    provider: &llm::AnthropicProvider,
    fallback: &[(&str, &str)],
) -> Vec<(String, String)> {
    pick_models(provider.list_models().await, fallback)
}

fn pick_models(
    fetched: Result<Vec<(String, String)>>,
    fallback: &[(&str, &str)],
) -> Vec<(String, String)> {
    match fetched {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => owned_models(fallback),
        Err(e) => {
            eprintln!("[models] falling back to built-in list: {e:#}");
            owned_models(fallback)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: &[(&str, &str)] = &[("Fallback", "fallback-1")];

    #[test]
    fn pick_models_prefers_a_nonempty_fetch() {
        let fetched = Ok(vec![("Fresh".to_string(), "fresh-1".to_string())]);
        assert_eq!(
            pick_models(fetched, FALLBACK),
            vec![("Fresh".to_string(), "fresh-1".to_string())]
        );
    }

    #[test]
    fn pick_models_falls_back_on_empty_fetch() {
        assert_eq!(
            pick_models(Ok(Vec::new()), FALLBACK),
            owned_models(FALLBACK)
        );
    }

    #[test]
    fn pick_models_falls_back_on_fetch_error() {
        let fetched = Err(anyhow::anyhow!("network down"));
        assert_eq!(pick_models(fetched, FALLBACK), owned_models(FALLBACK));
    }
}
