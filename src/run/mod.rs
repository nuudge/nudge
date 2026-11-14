pub mod attach;
mod daemon;
mod local;
mod local_peer;

use anyhow::{Context, Result};
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

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
    if cli.daemon && (cli.watch || cli.peer) && config.relay.is_none() {
        anyhow::bail!("--watch and --peer mint relay pairing codes: set NUDGE_RELAY");
    }
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

    let socket_path = session_socket_path(&session.id)?;
    let listener = transport::bind_listener(&socket_path)?;

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
    // Built here because `core` names no transport.
    let dialer_registrar = peer_reg_tx.clone();
    let dialer_identity = peer_identity.clone();
    let dialer_self_id = session.id.clone();
    let dialer: core::PeerDialer =
        Box::new(move |target: String, self_broker: core::BrokerHandle| {
            Box::pin(dial_peer(
                target,
                dialer_self_id.clone(),
                self_broker,
                dialer_registrar.clone(),
                dialer_identity.clone(),
            ))
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
    let serving = tokio::spawn(transport::serve_socket(
        listener,
        host.broker_handle(),
        transport::PeerAccept {
            identity: peer_identity.clone(),
            registrar: peer_reg_tx.clone(),
        },
    ));

    if let Some(target) = &cli.connect {
        request_connect(&host.broker_handle(), who.clone(), target).await;
    }

    let result = if cli.daemon {
        let peer_accept = cli.peer.then(|| transport::PeerAccept {
            identity: peer_identity,
            registrar: peer_reg_tx,
        });
        daemon::run(host, &socket_path, relay, cli.watch, peer_accept).await
    } else if cli.peer_host {
        daemon::run_peer_host(host).await
    } else {
        local::run(host, ui_cfg, who, relay, peer_identity, peer_reg_tx).await
    };
    serving.abort();
    // The listener doesn't unlink the socket file on drop.
    let _ = std::fs::remove_file(&socket_path);
    result
}

// The user's own `/connect-peer`, so it dials and fails exactly like one; commands aren't
// echoed. Non-driving and non-answering, so the brief attach skews no attribution or grant.
async fn request_connect(broker: &core::BrokerHandle, who: ClientIdentity, target: &str) {
    let profile = core::ClientProfile {
        may_drive: false,
        may_answer_permissions: false,
        ..core::ClientProfile::human()
    };
    // The broker drains a dropped controller's buffered command before reaping it.
    if let Some(controller) = broker.attach_as(who, profile).await {
        let _ = controller
            .ui_tx
            .send(core::UiEvent::Command {
                line: format!("/connect-peer {target}"),
            })
            .await;
    }
}

// A pairing code is self-contained, so the relay
// path works with no local relay configured; anything else is a local session.
async fn dial_peer(
    target: String,
    self_id: String,
    self_broker: core::BrokerHandle,
    registrar: tokio::sync::mpsc::UnboundedSender<core::PeerRegistration>,
    who: ClientIdentity,
) -> Result<()> {
    if !transport::Pairing::is_code(&target) {
        return local_peer::connect_local(&target, &self_id, self_broker, registrar, who).await;
    }
    let pairing = transport::Pairing::decode(&target)?;
    let client = transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher);
    client.dial_peer(self_broker, registrar, who).await
}

fn session_socket_path(session_id: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let dir = PathBuf::from(home).join(".nudge").join("run");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("{session_id}.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dial_peer_routes_a_pairing_code_to_the_relay() {
        let bb = core::host::spawn_bare_broker(Vec::new());
        let (registrar, _rx) = tokio::sync::mpsc::unbounded_channel();
        let err = dial_peer(
            "nudge:not*base64".into(),
            "self".into(),
            bb.handle.clone(),
            registrar,
            ClientIdentity::human("h"),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
        bb.task.abort();
    }

    #[tokio::test]
    async fn startup_connect_reaches_the_loop_as_a_command_and_is_not_echoed() {
        use std::time::Duration;
        let mut bb = core::host::spawn_bare_broker(Vec::new());
        let mut watcher = bb
            .handle
            .attach_as(
                ClientIdentity::human("w"),
                core::ClientProfile::watch_only(),
            )
            .await
            .unwrap();

        request_connect(&bb.handle, ClientIdentity::human("me"), "nudge:secret").await;

        match tokio::time::timeout(Duration::from_secs(5), bb.loop_ui_rx.recv()).await {
            Ok(Some((_, core::UiEvent::Command { line }))) => {
                assert_eq!(line, "/connect-peer nudge:secret")
            }
            other => panic!("expected the /connect-peer command, got {other:?}"),
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), watcher.events.recv())
                .await
                .is_err(),
            "the command must not reach other controllers"
        );
        bb.task.abort();
    }
}
