pub mod attach;
mod daemon;
mod local;
mod local_peer;

use anyhow::Result;
use tracing::{info, warn};

use crate::cli::Cli;
use crate::coding;
use crate::config::Config;
use crate::core::store::SessionRow;
use crate::core::{self, AgentConfig, Backend, ClientIdentity};
use crate::llm;
use crate::logging;
use crate::models::{MODELS, owned_models, resolve_models};
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
    if cli.daemon && !cli.local && config.relay.is_none() {
        anyhow::bail!("--daemon serves the relay too: set NUDGE_RELAY, or pass --local");
    }
    let (session, entries, dropped) = match &cli.resume {
        None => (coding::open_new()?, Vec::new(), 0),
        Some(id) => {
            let r = coding::open_resume(id)?;
            (r.session, r.entries, r.dropped)
        }
    };

    let thinking_display = crate::config::resolve_thinking(cli.thinking.as_ref())?;
    let socket_path = coding::session_socket_path(&session.id)?;
    // Refuses before the log is opened, so a second host can't trim or mark a live
    // session's log. Two hosts racing through MCP setup both pass; bind_listener decides.
    if core::is_listening(&socket_path) {
        anyhow::bail!("session {} is already running", session.id);
    }
    let mode = if cli.daemon || cli.print_prompt {
        logging::Mode::Headless
    } else {
        logging::Mode::Tui
    };
    let mode = logging::init(&socket_path.with_extension("log"), mode)?;
    let parent = coding::parent_of(&session.id)?;
    info!(
        version = env!("CARGO_PKG_VERSION"),
        mode,
        session = %session.id,
        cwd = %session.cwd.display(),
        model = %config.model,
        thinking = %thinking_display,
        resume = cli.resume.as_deref().map(tracing::field::display),
        local = cli.local,
        needs_answerer = cli.needs_answerer,
        watch = cli.watch,
        peer = cli.peer,
        connect = cli.connect.is_some(),
        spawned_by = parent.as_ref().map(|p| tracing::field::display(&p.id)),
        "session host starting"
    );
    let size_warning = logging::size_warning(&coding::run_dir()?)?;
    if let Some(text) = &size_warning {
        warn!("{text}");
    }

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

    // Failures are skipped; a TUI user also sees them as Notices, since the log is out of view.
    let mut startup_errors = Vec::new();
    let mcp_specs = match coding::mcp::load_config(&session.cwd) {
        Ok(specs) => specs,
        Err(e) => {
            startup_errors.push(format!("[mcp] config error: {e:#}"));
            Vec::new()
        }
    };
    let mcp = coding::mcp::McpRegistry::bootstrap(&mcp_specs).await;
    for line in &mcp.connect_log {
        info!("{line}");
    }
    startup_errors.extend(mcp.connect_errors.iter().cloned());

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let skills = coding::skills::SkillRegistry::discover(&session.cwd, home.as_deref());
    for line in &skills.discovery_log {
        info!("{line}");
    }
    startup_errors.extend(skills.discovery_errors.iter().cloned());
    for line in &startup_errors {
        warn!("{line}");
    }

    if cli.print_prompt {
        return coding::print_preamble(&cfg, &provider, &session, &mcp, &skills, config.web_search)
            .await;
    }

    let listener = transport::bind_listener(&socket_path)?;

    let role = subagent_role(parent.is_some());
    let mut backend =
        coding::CodingBackend::new(session.cwd.clone(), mcp, skills, config.web_search);

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
    // Agents never receive Notices, so a subagent's model isn't told.
    if !cli.daemon {
        let notices = startup_errors.into_iter().chain(size_warning);
        seed.extend(notices.map(|text| core::ControllerEvent::Notice { text }));
    }
    let peer_identity = core::ClientIdentity {
        kind: core::ClientKind::Agent,
        name: peer_name(session.name.as_deref(), &session.id),
        session_id: Some(session.id.clone()),
        task: None,
    };
    // A child gets no factory, so subagents can't spawn their own.
    let factory = match role {
        Some(role) => {
            backend = backend.into_subagent(role);
            None
        }
        None => {
            let self_id = session.id.clone();
            let who = peer_identity.clone();
            let factory: core::PeerFactory = Box::new(move |request, self_broker| {
                Box::pin(local_peer::spawn_child(
                    request,
                    self_id.clone(),
                    self_broker,
                    who.clone(),
                ))
            });
            Some(factory)
        }
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
            factory,
            initial_peers: Default::default(),
            register_rx: Some(peer_reg_rx),
            dialer: Some(dialer),
            lister: Some(Box::new(|| {
                Box::pin(async {
                    Ok(coding::list_sessions()?
                        .into_iter()
                        .map(|l| core::SessionChoice {
                            id: l.id,
                            name: l.name,
                            live: l.live,
                            turns: l.turns.max(0) as u64,
                            last_activity: l.last_activity.timestamp(),
                            connected: false,
                        })
                        .collect())
                })
            })),
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
        let relay = relay.filter(|_| !cli.local);
        daemon::run(
            host,
            &socket_path,
            relay,
            cli.watch,
            peer_accept,
            cli.needs_answerer,
        )
        .await
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

// How a session names itself on its peer edges.
fn peer_name(name: Option<&str>, id: &str) -> String {
    name.map(String::from)
        .unwrap_or_else(|| id.chars().take(8).collect())
}

// A session whose row records a parent is a child, however it was started.
fn subagent_role(has_parent: bool) -> Option<String> {
    has_parent.then(|| coding::prompt::SUBAGENT_ROLE.to_string())
}

// What /connect-peer, --connect and --attach accept. A pairing code is self-contained,
// so the relay path works with no local relay configured; anything else is a local
// session id (any project) or name (this cwd).
enum Target {
    Relay(transport::Pairing),
    Local(SessionRow),
}

fn parse_target(reference: &str) -> Result<Target> {
    if transport::Pairing::is_code(reference) {
        return Ok(Target::Relay(transport::Pairing::decode(reference)?));
    }
    coding::find_session(reference).map(Target::Local)
}

async fn dial_peer(
    target: String,
    self_id: String,
    self_broker: core::BrokerHandle,
    registrar: tokio::sync::mpsc::UnboundedSender<core::PeerRegistration>,
    who: ClientIdentity,
) -> Result<()> {
    let reg = match parse_target(&target)? {
        Target::Local(row) => {
            local_peer::connect_local(&target, row, &self_id, self_broker, who).await?
        }
        Target::Relay(pairing) => {
            transport::RelayClient::new(pairing.client_dial_url(), pairing.cipher)
                .dial_peer(self_broker, who)
                .await?
        }
    };
    registrar
        .send(reg)
        .map_err(|_| anyhow::anyhow!("the dialer's loop is gone"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_session_with_a_parent_gets_the_subagent_role() {
        assert!(
            subagent_role(false).is_none(),
            "a normal session is unchanged"
        );
        let role = subagent_role(true).unwrap();
        assert!(
            role.contains("marked 'your parent' in your peer roster"),
            "{role}"
        );
        assert!(
            !role.contains("{{"),
            "no parent name or id is baked in: {role}"
        );
    }

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
