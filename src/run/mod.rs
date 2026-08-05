pub mod connect;
mod daemon;
mod local;

use anyhow::Result;

use crate::cli::Cli;
use crate::coding;
use crate::config::Config;
use crate::core::{self, AgentConfig, Backend, ClientIdentity};
use crate::llm;
use crate::models::{DEFAULT_MODEL, MODELS, owned_models, resolve_models};
use crate::spawn;
use crate::transport;
use crate::tui;

pub const MAX_TOKENS: u32 = 16384;
pub const MAX_ITERATIONS: usize = 25;

// The local user's identity, announced at attach. `$USER` if set, else a neutral
// default — a `--name` override can come later.
pub fn local_identity() -> ClientIdentity {
    let name = std::env::var("USER").unwrap_or_else(|_| "human".into());
    ClientIdentity::human(name)
}

// Own a session and run its agent loop in-process: shared setup (session, provider,
// MCP, skills, host), then hand off to the headless daemon or the local TUI.
pub async fn host(cli: Cli) -> Result<()> {
    let config = Config::from_env()?;
    // Cloned before the provider takes ownership below, so a spawned child can
    // build its own provider with the same key.
    let api_key = config.anthropic_api_key.clone();

    let (session, entries, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.entries, r.dropped)
        }
    };

    let thinking_display = cli.thinking.as_display();
    let who = local_identity();
    let mut ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        model: DEFAULT_MODEL.into(),
        thinking_display: thinking_display.clone(),
        // Filled in the local branch when --relay arms remote pairing.
        pairing_qr: None,
        pairing_code: None,
        pairing_qr_watch: None,
        pairing_code_watch: None,
        pairing_qr_agent: None,
        pairing_code_agent: None,
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
        // Filled from the resolved catalog just below, once the provider exists.
        models: Vec::new(),
    };
    let provider = llm::AnthropicProvider::new(config.anthropic_api_key);

    ui_cfg.models = resolve_models(&provider, MODELS).await;
    let mut cfg = cfg;
    cfg.models = ui_cfg
        .models
        .iter()
        .map(|(label, id)| core::ModelInfo {
            id: id.clone(),
            label: label.clone(),
        })
        .collect();

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
    // replay. The model's conversation is rebuilt separately from the same
    // entries, with sender attribution applied at build time.
    let initial_messages = core::agent::resume_messages(&entries);
    let mut seed = coding::replay_events(&entries, dropped);
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
    // Seed the capability surface next to SessionInfo so every attach renders menus
    // (model picker, MCP catalog) from the daemon's data on its very first frame. The
    // loop re-emits it when the surface changes (MCP load/unload, peer change). A
    // fresh session holds no peers yet.
    seed.insert(
        1,
        core::ControllerEvent::Capabilities {
            commands: core::agent::command_catalog(),
            models: cfg.models.clone(),
            mcp: backend.mcp_catalog(),
            peers: Vec::new(),
        },
    );
    // The executor behind the model-facing Spawn tool: this session may create
    // subagents (which themselves may not — the factory builds children without one).
    let factory = spawn::peer_factory(api_key.clone(), session.id.clone());

    // This session's peer identity, announced on every peer edge: the renamed name if
    // set, else a short session id (#53). Agent-kind — a peer edge is agent-to-agent.
    let peer_identity = core::ClientIdentity {
        kind: core::ClientKind::Agent,
        name: session
            .name
            .clone()
            .unwrap_or_else(|| session.id.chars().take(8).collect()),
        session_id: Some(session.id.clone()),
        task: None,
    };
    // The runtime registrar (#52): its receiver goes to the loop, its sender to every
    // runtime producer — the /connect-peer dialer below (the forward edge) and, under
    // --daemon --peer, the agent leg (the reverse edge from a remote dialer).
    let (peer_reg_tx, peer_reg_rx) =
        tokio::sync::mpsc::unbounded_channel::<core::PeerRegistration>();
    // The human-only /connect-peer dialer (#53): decode the pasted code, dial the far
    // room with the reverse-edge offer, and register the forward edge via the registrar.
    // The code is self-contained (relay URL + room + key), so this works even with no
    // local relay configured. `core` names no transport, so the closure is built here.
    let dialer_registrar = peer_reg_tx.clone();
    let dialer_identity = peer_identity.clone();
    let dialer: core::PeerDialer =
        Box::new(move |code: String, self_broker: core::BrokerHandle| {
            let registrar = dialer_registrar.clone();
            let who = dialer_identity.clone();
            Box::pin(async move {
                let pairing = transport::Pairing::decode(&code)?;
                let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
                client.dial_peer(self_broker, registrar, who).await
            })
        });

    let host = core::SessionHost::spawn(
        cfg,
        provider,
        backend,
        session,
        initial_messages,
        seed,
        core::PeerWiring {
            factory: Some(factory),
            initial_peers: Default::default(),
            register_rx: Some(peer_reg_rx),
            dialer: Some(dialer),
        },
    );

    // The relay base URL for phone handoff (and the relay daemon). Optional: a plain
    // local session without it still runs and backgrounds — just no phone handoff.
    let relay = config.relay;

    if cli.daemon {
        // --daemon --peer parks an agent-scope leg that accepts a remote dialer's
        // reverse-edge offer, registering the return edge into this loop (#53).
        let peer_accept = cli.peer.then(|| transport::PeerAccept {
            identity: peer_identity,
            registrar: peer_reg_tx,
        });
        daemon::run(host, cli.socket, relay, cli.watch, peer_accept).await
    } else {
        // The co-located handoff arms an agent-peer leg too (#61): once /background
        // dials, this interactive session is simultaneously human-driven and dialable
        // via /connect-peer, so it needs the same identity + registrar the dialer got.
        local::run(host, ui_cfg, who, relay, peer_identity, peer_reg_tx).await
    }
}
