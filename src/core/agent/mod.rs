use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::mpsc;

use super::events::{AgentEvent, UiEvent};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request, SystemBlock};

mod dispatch;
mod naming;

use dispatch::dispatch_tools;
use naming::{fallback_name, short_id, title_from_response, title_prompt};

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_iterations: usize,
    // "summarized" (default) shows thinking text; "omitted" sends signature only
    // (faster TTFT). Cost is identical — this only changes wire-level visibility.
    pub thinking_display: String,
}

// Everything the loop needs from the concrete agent (tools, prompt/context,
// control handling). The coding agent implements this; the loop stays unaware
// of tool implementations, MCP, CLAUDE.md, or any cwd-specific concern — that
// is what keeps this module from depending on the layer above it.
pub trait Backend {
    // Rebuilt each turn so volatile context (env, git, dir listing) stays fresh.
    fn system_blocks(&self) -> Vec<SystemBlock>;
    // Tool schemas, plus the stable/dynamic cache-boundary index (None = no caching).
    fn tool_schemas(&self) -> (Vec<Value>, Option<usize>);
    fn tool_summary(&self, name: &str, input: &Value) -> String;
    fn requires_permission(&self, name: &str) -> bool;
    fn permission_summary(&self, name: &str, input: &Value) -> String;
    // May emit events (e.g. an OAuth URL) through `notify`.
    fn execute(
        &mut self,
        name: &str,
        input: &Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = Result<String>> + Send;
    // A control event the loop doesn't own (e.g. MCP load/unload/list); true if consumed.
    fn handle_control(
        &mut self,
        ev: &UiEvent,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = bool> + Send;
    // Re-read each turn boundary so a mid-session `git checkout` reaches the header.
    fn git_branch(&self) -> Option<String> {
        None
    }
}

pub async fn run_agent<P: Provider, B: Backend>(
    mut cfg: AgentConfig,
    provider: P,
    mut backend: B,
    mut session: Session,
    initial_messages: Vec<Message>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
    agent_tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
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
            match ui_rx.recv().await {
                Some(UiEvent::UserMessage { text }) => break text,
                Some(UiEvent::SetModel { model }) => {
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
                Some(UiEvent::RenameSession { name: requested }) => {
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
                Some(UiEvent::Quit) | None => return Ok(()),
                Some(ev) => {
                    backend.handle_control(&ev, &agent_tx).await;
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
            let (tools, tool_cache_boundary) = backend.tool_schemas();
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

            let (mut tool_results, denied) =
                dispatch_tools(&assistant_msg, &agent_tx, &mut backend).await;
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
                        Some(UiEvent::UserMessage { text }) => {
                            tool_results.push(ContentBlock::Text { text });
                            break;
                        }
                        Some(UiEvent::SetModel { model }) => {
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
                        Some(UiEvent::Quit) | None => return Ok(()),
                        Some(ev) => {
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

// Emit SessionInfo only when the (model, branch, name) tuple changed, so headers track
// the daemon without an identical event flooding the replay buffer each turn. `branch`
// is read by the caller *before* the await — holding `&Backend` across it would force a
// `B: Sync` bound on the whole loop.
async fn emit_session_info_if_changed(
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    branch: Option<String>,
    session: &Session,
    last: &mut (String, Option<String>, Option<String>),
) {
    let current = (model.to_string(), branch, session.name.clone());
    if current == *last {
        return;
    }
    *last = current.clone();
    let _ = tx
        .send(AgentEvent::SessionInfo {
            model: current.0,
            cwd: session.cwd_display(),
            git_branch: current.1,
            session_id: session.id.clone(),
            session_name: current.2,
        })
        .await;
}

// Persist a resolved label and broadcast it: a Notice (so the user — and the TUI, which
// can't know a daemon-derived name synchronously — sees the final label) plus a re-emit
// of SessionInfo through the change-detecting helper.
async fn finalize_rename(
    name: String,
    branch: Option<String>,
    cfg: &AgentConfig,
    session: &mut Session,
    tx: &mpsc::Sender<AgentEvent>,
    last_ctx: &mut (String, Option<String>, Option<String>),
) {
    match session.set_name(name.clone(), branch.clone()) {
        Ok(()) => {
            let _ = tx
                .send(AgentEvent::Notice {
                    text: format!("session renamed to '{name}'"),
                })
                .await;
            emit_session_info_if_changed(tx, &cfg.model, branch, session, last_ctx).await;
        }
        Err(e) => {
            let _ = tx
                .send(AgentEvent::Error {
                    message: format!("rename failed: {e:#}"),
                })
                .await;
        }
    }
}
