use anyhow::Result;

use super::events::{AgentEvent, UiEvent};
use crate::core::host::BrokerHandle;
use crate::core::identity::ClientIdentity;
use crate::core::peer::{PeerDialer, PeerSet};
use crate::core::session::{LoggedMessage, Session};
use crate::llm::{ContentBlock, Message, Provider, Request};

mod command;
mod dispatch;
mod naming;
mod peer_tools;
mod session_info;
mod steering;
mod supervision;
#[cfg(test)]
mod tests;
mod types;

pub use command::{McpCommand, command_catalog};
pub use types::{AgentConfig, AgentIo, Backend, LoopInput};

use command::Command;
use dispatch::dispatch_tools;
use naming::{fallback_name, short_id, title_from_response, title_prompt};
use session_info::{emit_session_info_if_changed, finalize_rename};
use supervision::{attribute_message, recv_registration, supervise_peer_event};

pub async fn run_agent<P: Provider, B: Backend>(
    mut cfg: AgentConfig,
    provider: P,
    mut backend: B,
    mut session: Session,
    initial_messages: Vec<Message>,
    io: AgentIo,
) -> Result<()> {
    let AgentIo {
        mut ui_rx,
        agent_tx,
        mut peers,
        mut peer_register_rx,
        peer_factory,
        peer_dialer,
        self_handle,
    } = io;
    let mut messages: Vec<Message> = initial_messages;
    // Index of `messages` after the last completed turn. On API error mid-turn we roll
    // back here so the next turn lands on a valid alternating-role boundary (the API
    // rejects consecutive user turns, or a tool_use turn missing its tool_results).
    let mut last_good_snapshot: usize = messages.len();

    // Last (model, git_branch, name) emitted as SessionInfo, so we only re-emit on an
    // actual change (a /model switch, a `git checkout`, a /session-rename).
    let mut last_session_ctx = (
        cfg.model.clone(),
        backend.git_branch(),
        session.name.clone(),
    );

    // OUTER loop: one iteration per user turn.
    loop {
        let (who, user_text) = loop {
            tokio::select! {
                ui = ui_rx.recv() => match ui {
                Some((who, UiEvent::UserMessage { text })) => break (who, text),
                Some((_, UiEvent::Command { line })) => {
                    dispatch_command(
                        &line,
                        &mut cfg,
                        &provider,
                        &mut backend,
                        &mut session,
                        &messages,
                        &peers,
                        &agent_tx,
                        &mut last_session_ctx,
                        &self_handle,
                        &peer_dialer,
                    )
                    .await;
                }
                Some((_, UiEvent::Quit)) | None => return Ok(()),
                // Terminated at the broker; never forwarded to the loop.
                Some((_, UiEvent::PermissionResponse { .. })) => {}
                },
                reg = recv_registration(&mut peer_register_rx) => match reg {
                    Some(reg) => {
                        // Fired only for runtime edges (a /connect-peer forward edge, or
                        // an inbound remote dialer's reverse edge) — so this Notice
                        // resolves the "connecting…" line on both sides and names the peer.
                        let name = reg.who.name.clone();
                        peers.register(reg);
                        let _ = agent_tx
                            .send(AgentEvent::Notice {
                                text: format!("connected to peer {name}"),
                            })
                            .await;
                        let _ = agent_tx
                            .send(capabilities_event(&cfg, &backend, &peers))
                            .await;
                    }
                    None => peer_register_rx = None,
                },
                (pid, ev) = peers.recv() => {
                    let roster_before = peers.roster();
                    // A supervised peer's check-in comes back here and is decided by
                    // one steering inference over this agent's own transcript.
                    if let supervision::Observed::CheckIn(checkin) =
                        supervise_peer_event(&mut peers, &agent_tx, pid, ev).await
                    {
                        steering::run_steering_turn(
                            &cfg,
                            &provider,
                            &backend,
                            &mut session,
                            &mut messages,
                            &mut last_good_snapshot,
                            &mut peers,
                            &agent_tx,
                            &peer_factory,
                            checkin,
                        )
                        .await?;
                    }
                    // A disconnected peer was reaped above — re-advertise the roster.
                    if peers.roster() != roster_before {
                        let _ = agent_tx
                            .send(capabilities_event(&cfg, &backend, &peers))
                            .await;
                    }
                }
            }
        };

        // The log stores the clean text + sender; the model-facing transcript gets
        // the attributed form, derived here at build time (and again on resume).
        messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: user_text }],
        });
        session.stage(messages.last().unwrap(), who.as_ref());
        attribute_message(who.as_ref(), messages.last_mut().unwrap());

        // INNER loop: model + tool turns until non-tool-use stop.
        for iteration in 0..cfg.max_iterations {
            let (mut tools, tool_cache_boundary) = backend.tool_schemas();
            // Loop-level peer tools ride after the backend's array (never inside the
            // cached stable prefix); offered per capability (factory / held peers).
            tools.extend(peer_tools::schemas(&peers, &peer_factory));
            // The peer roster rides as a trailing, uncached system block after the
            // backend's volatile env breakpoint, so the model knows whom it can address
            // and a peer joining/leaving never busts the cached stable prefix.
            let mut system = backend.system_blocks();
            if let Some(text) = peer_tools::roster_system_text(&peers) {
                system.push(crate::llm::SystemBlock { text, cache: false });
            }
            let req = Request {
                model: &cfg.model,
                max_tokens: cfg.max_tokens,
                thinking_display: &cfg.thinking_display,
                system,
                tools,
                tool_cache_boundary,
                tool_choice: None,
                messages: &messages,
            };

            let resp = match provider.complete(&req).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = agent_tx
                        .send(AgentEvent::Error {
                            message: format!("{e:#}"),
                        })
                        .await;
                    messages.truncate(last_good_snapshot);
                    session.rollback();
                    break;
                }
            };

            let _ = agent_tx
                .send(AgentEvent::Usage {
                    in_tokens: resp.usage.input_tokens,
                    out_tokens: resp.usage.output_tokens,
                    cache_write: resp.usage.cache_creation_input_tokens,
                    cache_read: resp.usage.cache_read_input_tokens,
                })
                .await;

            let assistant_msg = Message {
                role: "assistant".into(),
                content: resp.content,
            };
            session.stage(&assistant_msg, None);

            for block in &assistant_msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        let _ = agent_tx
                            .send(AgentEvent::AssistantText { text: text.clone() })
                            .await;
                    }
                    ContentBlock::Thinking { thinking, .. } if !thinking.is_empty() => {
                        let _ = agent_tx
                            .send(AgentEvent::AssistantThinking {
                                text: thinking.clone(),
                            })
                            .await;
                    }
                    _ => {}
                }
            }

            if resp.stop_reason != "tool_use" {
                messages.push(assistant_msg);
                last_good_snapshot = messages.len();
                session.commit().await?;
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
                break;
            }

            let roster_before = peers.roster();
            let (mut tool_results, denied) = dispatch_tools(
                &assistant_msg,
                &agent_tx,
                &mut backend,
                &mut peers,
                &peer_factory,
                &self_handle,
            )
            .await;
            // A Spawn/DismissPeer in the batch changed the roster — re-advertise it.
            if peers.roster() != roster_before {
                let _ = agent_tx
                    .send(capabilities_event(&cfg, &backend, &peers))
                    .await;
            }
            messages.push(assistant_msg);

            // After a denial, pause for fresh user guidance that rides along in the
            // same tool_results turn, so the model sees "denied — try this" in one step.
            let mut guidance_sender: Option<ClientIdentity> = None;
            if denied {
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
                loop {
                    match ui_rx.recv().await {
                        Some((who, UiEvent::UserMessage { text })) => {
                            guidance_sender = who;
                            tool_results.push(ContentBlock::Text { text });
                            break;
                        }
                        Some((_, UiEvent::Command { line })) => {
                            dispatch_command(
                                &line,
                                &mut cfg,
                                &provider,
                                &mut backend,
                                &mut session,
                                &messages,
                                &peers,
                                &agent_tx,
                                &mut last_session_ctx,
                                &self_handle,
                                &peer_dialer,
                            )
                            .await;
                        }
                        // Quit here drops any entries staged this turn (the tool_use
                        // assistant + earlier iterations). That's intentional, not a
                        // leak: the turn never committed, so on resume strict truncation
                        // would have discarded those trailing entries anyway.
                        Some((_, UiEvent::Quit)) | None => return Ok(()),
                        Some((_, UiEvent::PermissionResponse { .. })) => {}
                    }
                }
            }

            let mut user_msg = Message {
                role: "user".into(),
                content: tool_results,
            };
            session.stage(&user_msg, guidance_sender.as_ref());
            attribute_message(guidance_sender.as_ref(), &mut user_msg);
            messages.push(user_msg);

            if iteration == cfg.max_iterations - 1 {
                // Close the maxed-out turn on a valid alternating-role boundary —
                // otherwise it ends on user(tool_results) and the next user turn would
                // be two consecutive user turns, which the API rejects.
                let notice = format!(
                    "I've reached the iteration limit ({}) for this turn. The work above is partial. Tell me how you'd like to proceed, or reply 'continue' to resume.",
                    cfg.max_iterations
                );
                let synthetic = Message {
                    role: "assistant".into(),
                    content: vec![ContentBlock::Text {
                        text: notice.clone(),
                    }],
                };
                session.stage(&synthetic, None);
                let _ = agent_tx
                    .send(AgentEvent::AssistantText { text: notice })
                    .await;
                messages.push(synthetic);
                last_good_snapshot = messages.len();
                session.commit().await?;
                let _ = agent_tx.send(AgentEvent::MaxIterations).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    &session,
                    &mut last_session_ctx,
                )
                .await;
            }
        }
    }
}

// The Capabilities event, assembled from the static command grammar, the resolved
// model catalog, the backend's live MCP catalog, and the held peer roster.
// Re-emitted when the surface changes (MCP load/unload, peer change) so clients
// re-render menus.
fn capabilities_event<B: Backend>(cfg: &AgentConfig, backend: &B, peers: &PeerSet) -> AgentEvent {
    AgentEvent::Capabilities {
        commands: command_catalog(),
        models: cfg.models.clone(),
        mcp: backend.mcp_catalog(),
        peers: peers.infos(),
    }
}

// Execute one parsed `/…` command against the loop's state. Results ride back as
// Notice/SessionInfo/Capabilities events — the same shape a human, a --connect TUI,
// and the phone all see, since the parse lives here rather than in any front-end.
#[allow(clippy::too_many_arguments)]
async fn dispatch_command<P: Provider, B: Backend>(
    line: &str,
    cfg: &mut AgentConfig,
    provider: &P,
    backend: &mut B,
    session: &mut Session,
    messages: &[Message],
    peers: &PeerSet,
    agent_tx: &tokio::sync::mpsc::Sender<AgentEvent>,
    last_session_ctx: &mut (String, Option<String>, Option<String>),
    self_handle: &BrokerHandle,
    peer_dialer: &Option<PeerDialer>,
) {
    match command::parse(line) {
        Command::SetModel(model) => {
            cfg.model = model;
            emit_session_info_if_changed(
                agent_tx,
                &cfg.model,
                backend.git_branch(),
                session,
                last_session_ctx,
            )
            .await;
        }
        Command::Rename(requested) => {
            let branch = backend.git_branch();
            let name = match requested {
                // Tier 1: explicit name, used verbatim (trimmed).
                Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                // Tier 2: inside a git repo, branch + a short id so two sessions on
                // the same branch don't share a label.
                _ => match &branch {
                    Some(b) => format!("{}-{}", b, short_id(&session.id)),
                    // Tier 3: no repo — ask the model for a title. Awaited inline to
                    // keep the loop future `Send`.
                    None => match title_prompt(messages) {
                        Some(prompt) => {
                            let probe = [Message {
                                role: "user".into(),
                                content: vec![ContentBlock::Text { text: prompt }],
                            }];
                            let req = Request {
                                model: &cfg.model,
                                max_tokens: 1024,
                                thinking_display: "omitted",
                                system: Vec::new(),
                                tools: Vec::new(),
                                tool_cache_boundary: None,
                                tool_choice: None,
                                messages: &probe,
                            };
                            provider
                                .complete(&req)
                                .await
                                .ok()
                                .and_then(|r| title_from_response(&r))
                                .unwrap_or_else(|| fallback_name(session))
                        }
                        None => fallback_name(session),
                    },
                },
            };
            finalize_rename(name, branch, cfg, session, agent_tx, last_session_ctx).await;
        }
        Command::Mcp(mcp) => {
            let text = backend.handle_mcp(&mcp, agent_tx).await;
            let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            // A load/unload mutates the tool surface — re-advertise the catalog. A
            // bare list doesn't change anything, so it doesn't.
            if matches!(mcp, McpCommand::Load(_) | McpCommand::Unload(_)) {
                let _ = agent_tx.send(capabilities_event(cfg, backend, peers)).await;
            }
        }
        Command::Peers => {
            let infos = peers.infos();
            let text = if infos.is_empty() {
                "no peers held".to_string()
            } else {
                let mut lines = vec!["peers:".to_string()];
                for p in infos {
                    let kind = match p.kind {
                        crate::core::identity::ClientKind::Agent => "agent",
                        crate::core::identity::ClientKind::Human => "human",
                    };
                    let role = if p.supervised {
                        "supervised"
                    } else {
                        "unsupervised"
                    };
                    let mut line = format!("- {} ({kind}, {role})", p.name);
                    if let Some(id) = &p.session_id {
                        line.push_str(&format!(" — session {id}"));
                    }
                    if let Some(task) = &p.task {
                        let t: String = task.chars().take(80).collect();
                        let ellipsis = if t.len() < task.len() { "…" } else { "" };
                        line.push_str(&format!(" — task: {t}{ellipsis}"));
                    }
                    lines.push(line);
                }
                lines.join("\n")
            };
            let _ = agent_tx.send(AgentEvent::Notice { text }).await;
        }
        Command::ModelUsage => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: "usage: /model <id>".into(),
                })
                .await;
        }
        // Dial a remote peer over the relay. Human-only (no model tool) and never
        // blocks the loop: hand the code to the injected dialer and spawn it — the
        // finished forward edge arrives via `peer_register_rx`, and the reverse edge
        // (the far side driving us) attaches to our own broker. A failure comes back
        // as a Notice from the spawned task.
        Command::ConnectPeer(code) => {
            if code.trim().is_empty() {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: "usage: /connect-peer <pairing-code>".into(),
                    })
                    .await;
            } else if let Some(dialer) = peer_dialer {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: "connecting to peer over the relay…".into(),
                    })
                    .await;
                let fut = dialer(code, self_handle.clone());
                let agent_tx = agent_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = fut.await {
                        let _ = agent_tx
                            .send(AgentEvent::Notice {
                                text: format!("could not connect to peer: {e:#}"),
                            })
                            .await;
                    }
                });
            } else {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: "peer connections are not available in this session".into(),
                    })
                    .await;
            }
        }
        Command::Unknown(cmd) => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("unknown command: {cmd}"),
                })
                .await;
        }
    }
}

// Rebuild the model-facing transcript from logged entries: the log stores clean
// text + sender, and attribution is derived here exactly as the live path derives
// it at arrival. Pre-sender logs have `sender: None` (their text may carry an
// already-baked prefix), which `attribute` leaves untouched.
pub fn resume_messages(entries: &[LoggedMessage]) -> Vec<Message> {
    entries
        .iter()
        .map(|e| {
            let mut msg = e.message.clone();
            attribute_message(e.sender.as_ref(), &mut msg);
            msg
        })
        .collect()
}
