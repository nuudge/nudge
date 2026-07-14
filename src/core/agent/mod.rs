use anyhow::Result;

use super::events::{AgentEvent, UiEvent};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request};

mod dispatch;
mod naming;
mod peer_tools;
mod session_info;
mod supervision;
#[cfg(test)]
mod tests;
mod types;

pub use types::{AgentConfig, AgentIo, Backend, LoopInput};

use dispatch::dispatch_tools;
use naming::{fallback_name, short_id, title_from_response, title_prompt};
use session_info::{emit_session_info_if_changed, finalize_rename};
use supervision::{attribute, recv_registration, supervise_peer_event};

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
        let user_text = loop {
            tokio::select! {
                ui = ui_rx.recv() => match ui {
                Some((who, UiEvent::UserMessage { text })) => break attribute(who.as_ref(), text),
                Some((_, UiEvent::SetModel { model })) => {
                    cfg.model = model;
                    emit_session_info_if_changed(
                        &agent_tx,
                        &cfg.model,
                        backend.git_branch(),
                        &session,
                        &mut last_session_ctx,
                    )
                    .await;
                }
                Some((_, UiEvent::RenameSession { name: requested })) => {
                    let branch = backend.git_branch();
                    let name = match requested {
                        // Tier 1: explicit name, used verbatim (trimmed).
                        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                        // Tier 2: inside a git repo, branch + a short id so two
                        // sessions on the same branch don't share a label.
                        _ => match &branch {
                            Some(b) => format!("{}-{}", b, short_id(&session.id)),
                            // Tier 3: no repo — ask the model for a title. Awaited
                            // inline (not via a `&P`-capturing helper) to keep the loop
                            // future `Send`.
                            None => match title_prompt(&messages) {
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
                                        messages: &probe,
                                    };
                                    provider
                                        .complete(&req)
                                        .await
                                        .ok()
                                        .and_then(|r| title_from_response(&r))
                                        .unwrap_or_else(|| fallback_name(&session))
                                }
                                None => fallback_name(&session),
                            },
                        },
                    };
                    finalize_rename(
                        name,
                        branch,
                        &cfg,
                        &mut session,
                        &agent_tx,
                        &mut last_session_ctx,
                    )
                    .await;
                }
                Some((_, UiEvent::Quit)) | None => return Ok(()),
                Some((_, ev)) => {
                    backend.handle_control(&ev, &agent_tx).await;
                }
                },
                reg = recv_registration(&mut peer_register_rx) => match reg {
                    Some(reg) => {
                        peers.register(reg);
                    }
                    None => peer_register_rx = None,
                },
                (pid, ev) = peers.recv() => {
                    supervise_peer_event(&mut peers, &agent_tx, pid, ev).await;
                }
            }
        };

        messages.push(Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: user_text }],
        });
        session.log(messages.last().unwrap()).await?;

        // INNER loop: model + tool turns until non-tool-use stop.
        for iteration in 0..cfg.max_iterations {
            let (mut tools, tool_cache_boundary) = backend.tool_schemas();
            // Loop-level peer tools ride after the backend's array (never inside the
            // cached stable prefix); offered per capability (factory / held peers).
            tools.extend(peer_tools::schemas(&peers, &peer_factory));
            let req = Request {
                model: &cfg.model,
                max_tokens: cfg.max_tokens,
                thinking_display: &cfg.thinking_display,
                system: backend.system_blocks(),
                tools,
                tool_cache_boundary,
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
            session.log(&assistant_msg).await?;

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

            let (mut tool_results, denied) = dispatch_tools(
                &assistant_msg,
                &agent_tx,
                &mut backend,
                &mut peers,
                &peer_factory,
                &self_handle,
            )
            .await;
            messages.push(assistant_msg);

            // After a denial, pause for fresh user guidance that rides along in the
            // same tool_results turn, so the model sees "denied — try this" in one step.
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
                            tool_results.push(ContentBlock::Text {
                                text: attribute(who.as_ref(), text),
                            });
                            break;
                        }
                        Some((_, UiEvent::SetModel { model })) => {
                            cfg.model = model;
                            emit_session_info_if_changed(
                                &agent_tx,
                                &cfg.model,
                                backend.git_branch(),
                                &session,
                                &mut last_session_ctx,
                            )
                            .await;
                        }
                        Some((_, UiEvent::Quit)) | None => return Ok(()),
                        Some((_, ev)) => {
                            backend.handle_control(&ev, &agent_tx).await;
                        }
                    }
                }
            }

            let user_msg = Message {
                role: "user".into(),
                content: tool_results,
            };
            session.log(&user_msg).await?;
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
                session.log(&synthetic).await?;
                let _ = agent_tx
                    .send(AgentEvent::AssistantText { text: notice })
                    .await;
                messages.push(synthetic);
                last_good_snapshot = messages.len();
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
