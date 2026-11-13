pub mod connect;
mod daemon;
mod local;

use anyhow::Result;

use crate::cli::Cli;
use crate::coding;
use crate::config::Config;
use crate::core::{self, AgentConfig, Backend, ClientIdentity};
use crate::llm;
use crate::models::{MODELS, owned_models, resolve_models};
use crate::spawn;
use crate::transport;
use crate::tui;

pub const MAX_TOKENS: u32 = 16384;
pub const MAX_ITERATIONS: usize = 50;

pub fn local_identity() -> ClientIdentity {
    let name = std::env::var("NUDGE_NAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "human".into());
    ClientIdentity::human(name)
}

pub async fn host(cli: Cli) -> Result<()> {
    let config = Config::from_env()?;
    // A spawned child builds its own provider with the same key.
    let api_key = config.anthropic_api_key.clone();

    let (session, entries, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.entries, r.dropped)
        }
    };

    let thinking_display = crate::config::resolve_thinking(cli.thinking.as_ref())?;
    let who = local_identity();
    let mut ui_cfg = tui::UiConfig {
        session_id: session.id.clone(),
        session_name: session.name.clone(),
        auto: session.auto,
        model: config.model.clone(),
        thinking_display: thinking_display.clone(),
        pairing_qr: None,
        pairing_code: None,
        pairing_qr_watch: None,
        pairing_code_watch: None,
        pairing_qr_agent: None,
        pairing_code_agent: None,
        is_owner: true,
        user_name: who.name.clone(),
        models: owned_models(MODELS),
    };
    let cfg = AgentConfig {
        model: config.model.clone(),
        max_tokens: MAX_TOKENS,
        max_iterations: config.max_iterations,
        thinking_display,
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

    // MCP and skills load before the TUI takes the screen so their logs print
    // cleanly to stderr. Failures are logged and skipped.
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

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let skills = coding::skills::SkillRegistry::discover(&session.cwd, home.as_deref());
    for line in &skills.discovery_log {
        eprintln!("{line}");
    }

    if cli.print_prompt {
        return coding::print_preamble(&cfg, &provider, &session, &mcp, &skills, config.web_search)
            .await;
    }

    let backend = coding::CodingBackend::new(session.cwd.clone(), mcp, skills, config.web_search);

    // Front-ends rebuild history purely from the replayed event stream; the
    // model's conversation is rebuilt separately from the same entries.
    let initial_messages = core::agent::resume_messages(&entries);
    let mut seed = coding::replay_events(&entries, dropped);
    // SessionInfo and Capabilities go first so every attach renders its header
    // and menus on the very first frame.
    seed.insert(
        0,
        core::ControllerEvent::SessionInfo {
            model: cfg.model.clone(),
            cwd: session.cwd_display(),
            git_branch: backend.git_branch(),
            session_id: session.id.clone(),
            session_name: session.name.clone(),
            auto: session.auto,
        },
    );
    seed.insert(
        1,
        core::ControllerEvent::Capabilities {
            commands: core::agent::command_catalog(),
            models: cfg.models.clone(),
            mcp: backend.mcp_catalog(),
            peers: Vec::new(),
        },
    );
    // Children are built without a factory, so subagents can't spawn their own.
    let factory = spawn::peer_factory(
        api_key.clone(),
        session.id.clone(),
        config.model.clone(),
        config.max_iterations,
        config.web_search,
    );

    let peer_identity = core::ClientIdentity {
        kind: core::ClientKind::Agent,
        name: session
            .name
            .clone()
            .unwrap_or_else(|| session.id.chars().take(8).collect()),
        session_id: Some(session.id.clone()),
        task: None,
    };
    let (peer_reg_tx, peer_reg_rx) =
        tokio::sync::mpsc::unbounded_channel::<core::PeerRegistration>();
    // Built here because `core` names no transport. The pairing code is
    // self-contained, so this works with no local relay configured.
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

    let relay = config.relay;

    if cli.daemon {
        let peer_accept = cli.peer.then(|| transport::PeerAccept {
            identity: peer_identity,
            registrar: peer_reg_tx,
        });
        daemon::run(host, cli.socket, relay, cli.watch, peer_accept).await
    } else {
        local::run(host, ui_cfg, who, relay, peer_identity, peer_reg_tx).await
    }
}
