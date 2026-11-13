use anyhow::Result;

use super::events::{AgentEvent, UiEvent};
use crate::core::host::BrokerHandle;
use crate::core::identity::ClientIdentity;
use crate::core::peer::{PeerDialer, PeerFactory, PeerSet};
use crate::core::session::{LoggedMessage, Session};
use crate::llm::{
    CODE_EXECUTION_NOTICE, ContentBlock, Message, Provider, Request, WEB_FETCH_TOOL,
    WEB_SEARCH_TOOL, merge_text_runs, ran_code_execution, web_fetch_result, web_fetch_url,
    web_search_query, web_search_results,
};

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

pub(crate) use command::pre_approves_permissions;
pub use command::{McpCommand, command_catalog};
pub use types::{AgentConfig, AgentIo, Backend, LoopInput};

use command::Command;
use dispatch::dispatch_tools;
use naming::{fallback_name, short_id, title_from_response, title_prompt};
use session_info::{emit_session_info_if_changed, finalize_rename};
use supervision::{attribute_message, recv_registration, supervise_peer_event};

pub async fn run_agent<P: Provider, B: Backend>(
    cfg: AgentConfig,
    provider: P,
    backend: B,
    mut session: Session,
    initial_messages: Vec<Message>,
    io: AgentIo,
) -> Result<()> {
    let result = run_loop(cfg, provider, backend, &mut session, initial_messages, io).await;
    // Every graceful wind-down funnels through here; only a crash skips it, leaving the
    // row 'running' with last_activity as the staleness signal.
    let ended = session.end();
    result.and(ended)
}

async fn run_loop<P: Provider, B: Backend>(
    mut cfg: AgentConfig,
    provider: P,
    mut backend: B,
    session: &mut Session,
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
        multi_driver,
    } = io;
    let mut messages: Vec<Message> = initial_messages;
    // Rollback point on a mid-turn API error: the API rejects consecutive user turns and
    // tool_use turns missing their tool_results.
    let mut last_good_snapshot: usize = messages.len();

    let mut last_session_ctx = (
        cfg.model.clone(),
        backend.git_branch(),
        session.name.clone(),
        session.auto,
    );

    // A coalesce over-read stashes one input here; the next turn consumes it first.
    let mut pending_input: Option<LoopInput> = None;
    let mut last_context_cleared: Option<String> = None;
    let mut auto = session.auto;
    if auto {
        let _ = agent_tx
            .send(AgentEvent::Notice {
                text: "auto mode is on (restored from the last run) — /auto turns it off".into(),
            })
            .await;
    }
    loop {
        let (who, mut user_text) = loop {
            let input = match pending_input.take() {
                Some(p) => p,
                None => tokio::select! {
                    ui = ui_rx.recv() => match ui {
                        Some(input) => input,
                        None => return Ok(()),
                    },
                    reg = recv_registration(&mut peer_register_rx) => {
                        match reg {
                            Some(reg) => {
                                // Resolves the "connecting…" line on both sides.
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
                        }
                        continue;
                    }
                    (pid, ev) = peers.recv() => {
                        let roster_before = peers.roster();
                        if let supervision::Observed::CheckIn(checkin) =
                            supervise_peer_event(&mut peers, &agent_tx, pid, ev).await
                        {
                            steering::run_steering_turn(
                                &cfg,
                                &provider,
                                &backend,
                                session,
                                &mut messages,
                                &mut last_good_snapshot,
                                &mut peers,
                                &agent_tx,
                                &peer_factory,
                                auto,
                                checkin,
                            )
                            .await?;
                        }
                        if peers.roster() != roster_before {
                            let _ = agent_tx
                                .send(capabilities_event(&cfg, &backend, &peers))
                                .await;
                        }
                        continue;
                    }
                },
            };
            match input {
                (who, UiEvent::UserMessage { text }) => break (who, text),
                (_, UiEvent::Command { line }) => {
                    dispatch_command(
                        &line,
                        &mut cfg,
                        &provider,
                        &mut backend,
                        session,
                        &messages,
                        &peers,
                        &agent_tx,
                        &mut last_session_ctx,
                        &self_handle,
                        &peer_dialer,
                        &peer_factory,
                        &mut auto,
                    )
                    .await;
                }
                (_, UiEvent::Quit) => return Ok(()),
                // Terminated at the broker; never forwarded to the loop.
                (_, UiEvent::PermissionResponse { .. }) => {}
            }
        };

        // Coalesce already-queued messages from the same sender into one turn (fewer
        // tokens); the first non-mergeable input is stashed for the next turn.
        loop {
            match ui_rx.try_recv() {
                Ok((w, UiEvent::UserMessage { text })) if same_sender(&w, &who) => {
                    user_text.push_str("\n\n");
                    user_text.push_str(&text);
                }
                Ok(other) => {
                    pending_input = Some(other);
                    break;
                }
                Err(_) => break,
            }
        }

        // The log stores clean text + sender; only the model-facing copy is attributed.
        messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: user_text }],
        });
        session.stage(messages.last().unwrap(), who.as_ref());
        attribute_message(
            who.as_ref(),
            messages.last_mut().unwrap(),
            multi_driver.load(std::sync::atomic::Ordering::Relaxed),
        );

        // A pause_turn during server-side code execution must resume in the same
        // container; a paused response may not repeat its id, so keep the latest.
        let mut container: Option<String> = None;
        let mut resuming_pause = false;
        for iteration in 0..cfg.max_iterations {
            let (mut tools, tool_cache_boundary) = backend.tool_schemas();
            tools.extend(peer_tools::schemas(&peers, &peer_factory, auto));
            let mut system = backend.system_blocks();
            if let Some(text) = peer_tools::roster_system_text(&peers) {
                system.push(crate::llm::SystemBlock { text, cache: false });
            }
            if auto {
                system.push(peer_tools::auto_mode_system_block());
            }
            let req = Request {
                model: &cfg.model,
                max_tokens: cfg.max_tokens,
                thinking_display: &cfg.thinking_display,
                system,
                tools,
                tool_cache_boundary,
                web_search: backend.web_search(),
                container: if resuming_pause {
                    container.as_deref()
                } else {
                    None
                },
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
            if let Some(text) = resp.warning {
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
            if let Some(text) = resp.context_cleared
                && last_context_cleared.as_ref() != Some(&text)
            {
                last_context_cleared = Some(text.clone());
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
            if resp.container_id.is_some() {
                container = resp.container_id;
            }

            let assistant_msg = Message {
                role: "assistant".into(),
                content: resp.content,
            };
            session.stage(&assistant_msg, None);

            for block in merge_text_runs(&assistant_msg.content) {
                match block.as_ref() {
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
                    ContentBlock::ServerToolUse {
                        id, name, input, ..
                    } if name == WEB_SEARCH_TOOL => {
                        let _ = agent_tx
                            .send(AgentEvent::ToolUseStart {
                                id: id.clone(),
                                name: "WebSearch".into(),
                                summary: web_search_query(input),
                            })
                            .await;
                    }
                    ContentBlock::WebSearchToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let (content, is_error) = web_search_results(content);
                        let _ = agent_tx
                            .send(AgentEvent::ToolResult {
                                id: tool_use_id.clone(),
                                content,
                                is_error,
                            })
                            .await;
                    }
                    ContentBlock::ServerToolUse {
                        id, name, input, ..
                    } if name == WEB_FETCH_TOOL => {
                        let _ = agent_tx
                            .send(AgentEvent::ToolUseStart {
                                id: id.clone(),
                                name: "WebFetch".into(),
                                summary: web_fetch_url(input),
                            })
                            .await;
                    }
                    ContentBlock::WebFetchToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        let (content, is_error) = web_fetch_result(content);
                        let _ = agent_tx
                            .send(AgentEvent::ToolResult {
                                id: tool_use_id.clone(),
                                content,
                                is_error,
                            })
                            .await;
                    }
                    _ => {}
                }
            }
            if ran_code_execution(&assistant_msg.content) {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: CODE_EXECUTION_NOTICE.into(),
                    })
                    .await;
            }

            // The API's own server-tool loop hit its limit mid-turn; sending the
            // partial assistant turn back as the last message resumes it.
            resuming_pause = resp.stop_reason == "pause_turn";
            if resuming_pause && iteration + 1 < cfg.max_iterations {
                messages.push(assistant_msg);
                continue;
            }

            if resp.stop_reason != "tool_use" {
                messages.push(assistant_msg);
                last_good_snapshot = messages.len();
                session.commit()?;
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    session.id.clone(),
                    session.cwd_display(),
                    session.name.clone(),
                    session.auto,
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
                auto,
            )
            .await;
            if peers.roster() != roster_before {
                let _ = agent_tx
                    .send(capabilities_event(&cfg, &backend, &peers))
                    .await;
            }
            messages.push(assistant_msg);

            // After a denial, wait for guidance and send it in the same tool_results turn.
            let mut guidance_sender: Option<ClientIdentity> = None;
            if denied {
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    session.id.clone(),
                    session.cwd_display(),
                    session.name.clone(),
                    session.auto,
                    &mut last_session_ctx,
                )
                .await;
                loop {
                    let input = match pending_input.take() {
                        Some(p) => p,
                        None => match ui_rx.recv().await {
                            Some(input) => input,
                            None => return Ok(()),
                        },
                    };
                    match input {
                        (who, UiEvent::UserMessage { text }) => {
                            guidance_sender = who;
                            tool_results.push(ContentBlock::Text { text });
                            break;
                        }
                        (_, UiEvent::Command { line }) => {
                            dispatch_command(
                                &line,
                                &mut cfg,
                                &provider,
                                &mut backend,
                                session,
                                &messages,
                                &peers,
                                &agent_tx,
                                &mut last_session_ctx,
                                &self_handle,
                                &peer_dialer,
                                &peer_factory,
                                &mut auto,
                            )
                            .await;
                        }
                        // Dropping this turn's uncommitted staged entries is fine: resume
                        // truncation would discard them anyway.
                        (_, UiEvent::Quit) => return Ok(()),
                        (_, UiEvent::PermissionResponse { .. }) => {}
                    }
                }
            }

            let mut user_msg = Message {
                role: "user".into(),
                content: tool_results,
            };
            session.stage(&user_msg, guidance_sender.as_ref());
            attribute_message(
                guidance_sender.as_ref(),
                &mut user_msg,
                multi_driver.load(std::sync::atomic::Ordering::Relaxed),
            );
            messages.push(user_msg);

            if iteration == cfg.max_iterations - 1 {
                // End on an assistant turn: two consecutive user turns are rejected.
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
                session.commit()?;
                let _ = agent_tx.send(AgentEvent::MaxIterations).await;
                // Notices never reach a spawner's model, so wake it with a message or
                // the child stalls until a human notices.
                if let Some(spawner) = peers.spawner_id() {
                    peers
                        .drive(
                            spawner,
                            UiEvent::UserMessage {
                                text: format!(
                                    "I've hit my iteration limit ({}) for this turn; my work is paused and partial. Reply 'continue' via MessagePeer to let me resume, or ask me for a status report so you can decide what to do next.",
                                    cfg.max_iterations
                                ),
                            },
                        )
                        .await;
                }
                emit_session_info_if_changed(
                    &agent_tx,
                    &cfg.model,
                    backend.git_branch(),
                    session.id.clone(),
                    session.cwd_display(),
                    session.name.clone(),
                    session.auto,
                    &mut last_session_ctx,
                )
                .await;
            }
        }
    }
}

// Two unstamped (None) senders count as the same.
fn same_sender(a: &Option<ClientIdentity>, b: &Option<ClientIdentity>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x.name == y.name,
        (None, None) => true,
        _ => false,
    }
}

fn capabilities_event<B: Backend>(cfg: &AgentConfig, backend: &B, peers: &PeerSet) -> AgentEvent {
    AgentEvent::Capabilities {
        commands: command_catalog(),
        models: cfg.models.clone(),
        mcp: backend.mcp_catalog(),
        peers: peers.infos(),
    }
}

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
    last_session_ctx: &mut (String, Option<String>, Option<String>, bool),
    self_handle: &BrokerHandle,
    peer_dialer: &Option<PeerDialer>,
    peer_factory: &Option<PeerFactory>,
    auto: &mut bool,
) {
    match command::parse(line) {
        // Auto mode delegates through Spawn, so a session that cannot spawn can't use it.
        Command::Auto => {
            let text = if peer_factory.is_none() {
                "auto mode is not available in this session (it cannot spawn subagents)"
            } else {
                *auto = !*auto;
                if let Err(e) = session.set_auto(*auto) {
                    let _ = agent_tx
                        .send(AgentEvent::Error {
                            message: format!("couldn't save auto mode for resume: {e:#}"),
                        })
                        .await;
                }
                if *auto {
                    "auto mode on — work is delegated to a supervised subagent and nothing waits on you; skipped steps are listed in the report"
                } else {
                    "auto mode off"
                }
            };
            let _ = agent_tx
                .send(AgentEvent::Notice { text: text.into() })
                .await;
            emit_session_info_if_changed(
                agent_tx,
                &cfg.model,
                backend.git_branch(),
                session.id.clone(),
                session.cwd_display(),
                session.name.clone(),
                *auto,
                last_session_ctx,
            )
            .await;
        }
        Command::SetModel(model) => {
            cfg.model = model;
            emit_session_info_if_changed(
                agent_tx,
                &cfg.model,
                backend.git_branch(),
                session.id.clone(),
                session.cwd_display(),
                session.name.clone(),
                session.auto,
                last_session_ctx,
            )
            .await;
        }
        Command::Rename(requested) => {
            let branch = backend.git_branch();
            let name = match requested {
                Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                // The short id keeps two sessions on the same branch distinct.
                _ => match &branch {
                    Some(b) => format!("{}-{}", b, short_id(&session.id)),
                    // Awaited inline to keep the loop future `Send`.
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
                                web_search: false,
                                container: None,
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
        // Spawned so the dial never blocks the loop; the edge arrives via `peer_register_rx`.
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

// Multi-driver is inferred from the persisted senders. Old logs have `sender: None` with
// any prefix already baked into the text, which `attribute` leaves untouched.
pub fn resume_messages(entries: &[LoggedMessage]) -> Vec<Message> {
    let distinct: std::collections::HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.sender.as_ref().map(|w| w.name.as_str()))
        .filter(|n| !n.is_empty())
        .collect();
    let multi = distinct.len() >= 2;
    entries
        .iter()
        .map(|e| {
            let mut msg = e.message.clone();
            attribute_message(e.sender.as_ref(), &mut msg, multi);
            msg
        })
        .collect()
}
