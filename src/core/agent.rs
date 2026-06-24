use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::{mpsc, oneshot};

use super::events::{AgentEvent, UiEvent};
use crate::llm::{ContentBlock, Message, Provider, Request, SystemBlock};
use crate::core::session::Session;

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub max_iterations: usize,
    // "summarized" — visible thinking text in responses; default.
    // "omitted" — empty thinking field (signature only), faster TTFT.
    // Cost is identical either way; this only changes wire-level visibility.
    pub thinking_display: String,
}

// Everything the loop needs from the concrete agent (tools, prompt/context,
// control handling). The coding agent implements this; the loop stays unaware
// of tool implementations, MCP, CLAUDE.md, or any cwd-specific concern — that
// is what keeps this module from depending on the layer above it.
pub trait Backend {
    // System-prompt blocks for the upcoming request. Rebuilt each turn so
    // volatile context (env, git, dir listing) stays fresh.
    fn system_blocks(&self) -> Vec<SystemBlock>;
    // Tool schemas for the request, plus the index of the stable/dynamic cache
    // boundary (None disables tool-level caching).
    fn tool_schemas(&self) -> (Vec<Value>, Option<usize>);
    // Short label for the ToolUseStart event.
    fn tool_summary(&self, name: &str, input: &Value) -> String;
    // Whether a call to `name` must be approved by the user before running.
    fn requires_permission(&self, name: &str) -> bool;
    // Text shown in the approval prompt for this specific call.
    fn permission_summary(&self, name: &str, input: &Value) -> String;
    // Run the tool. May emit events (e.g. an OAuth URL) through `notify`.
    fn execute(
        &mut self,
        name: &str,
        input: &Value,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = Result<String>> + Send;
    // Handle a control event the loop doesn't own (e.g. MCP load/unload/list).
    // Returns true if consumed. The loop handles UserMessage/SetModel/Quit.
    fn handle_control(
        &mut self,
        ev: &UiEvent,
        notify: &mpsc::Sender<AgentEvent>,
    ) -> impl Future<Output = bool> + Send;
    // Current git branch of the agent's working dir, re-read by the loop at each turn
    // boundary so a mid-session `git checkout` reaches every controller's header. None
    // when the cwd isn't a repo. Default None for backends without a working dir.
    fn git_branch(&self) -> Option<String> {
        None
    }
}

pub async fn run_agent<P: Provider, B: Backend>(
    mut cfg: AgentConfig,
    provider: P,
    mut backend: B,
    session: Session,
    initial_messages: Vec<Message>,
    mut ui_rx: mpsc::Receiver<UiEvent>,
    agent_tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
    let mut messages: Vec<Message> = initial_messages;
    // Index of `messages` after the last fully-completed turn (assistant
    // returned a non-tool-use stop). On API error mid-turn we roll back here
    // so the next user message lands on a valid alternating-role boundary —
    // the API rejects two consecutive user turns, and a tool_use assistant
    // turn without its tool_results is also invalid. On resume, the loaded
    // messages have already been truncated to a clean boundary, so the
    // current length is itself a valid snapshot.
    let mut last_good_snapshot: usize = messages.len();

    // Last (model, git_branch) pushed as a SessionInfo, primed from the startup values
    // (which match main.rs's seed) so we only re-emit when one actually changes — a
    // `/model` switch or a mid-session `git checkout` — rather than once per turn.
    let mut last_session_ctx = (cfg.model.clone(), backend.git_branch());

    // OUTER loop: one iteration per user turn.
    loop {
        let user_text = loop {
            match ui_rx.recv().await {
                Some(UiEvent::UserMessage { text }) => break text,
                Some(UiEvent::SetModel { model }) => {
                    cfg.model = model;
                    emit_session_info_if_changed(&agent_tx, &cfg.model, backend.git_branch(), &session, &mut last_session_ctx).await;
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
                emit_session_info_if_changed(&agent_tx, &cfg.model, backend.git_branch(), &session, &mut last_session_ctx).await;
                break;
            }

            let (mut tool_results, denied) =
                dispatch_tools(&assistant_msg, &agent_tx, &mut backend).await;
            messages.push(assistant_msg);

            // Mirror current behavior: after any denial, pause for fresh user
            // guidance that rides along in the same tool_results turn so the
            // model sees "denied — try this instead" in one coherent step.
            if denied {
                let _ = agent_tx.send(AgentEvent::TurnComplete).await;
                emit_session_info_if_changed(&agent_tx, &cfg.model, backend.git_branch(), &session, &mut last_session_ctx).await;
                loop {
                    match ui_rx.recv().await {
                        Some(UiEvent::UserMessage { text }) => {
                            tool_results.push(ContentBlock::Text { text });
                            break;
                        }
                        Some(UiEvent::SetModel { model }) => {
                            cfg.model = model;
                            emit_session_info_if_changed(&agent_tx, &cfg.model, backend.git_branch(), &session, &mut last_session_ctx).await;
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
                // Without this synthetic turn, the inner loop exits with
                // `messages` ending on the just-pushed user(tool_results)
                // turn. The next outer-loop user message would then create
                // two consecutive user turns and the API would reject the
                // next request (last_good_snapshot rolls back on error, but
                // at the cost of a wasted API call and a confusing UX gap).
                // The synthetic acknowledgment closes the turn cleanly on a
                // valid alternating-role boundary AND tells the user what
                // happened in-conversation.
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
                emit_session_info_if_changed(&agent_tx, &cfg.model, backend.git_branch(), &session, &mut last_session_ctx).await;
            }
        }
    }
}

// Re-read the volatile context (model, git branch) and emit a SessionInfo only if it
// changed since the last emit, so every controller's header tracks the daemon without
// flooding the replay buffer with an identical event each turn. cwd/session-id are
// fixed for the session. git_branch() re-runs git, so call this at turn boundaries.
// `branch` is read by the caller *before* the await (so no `&Backend` is held across
// it — that would force a `B: Sync` bound on the whole loop). model is borrowed; the
// rest are owned/Send.
async fn emit_session_info_if_changed(
    tx: &mpsc::Sender<AgentEvent>,
    model: &str,
    branch: Option<String>,
    session: &Session,
    last: &mut (String, Option<String>),
) {
    let current = (model.to_string(), branch);
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
        })
        .await;
}

// Execute every tool call in the assistant turn: emit start/result events,
// gate on permission (one modal prompt at a time), short-circuit the rest of
// the batch after a denial, and return one ToolResult per call (the API
// requires it). The concrete classification/execution is delegated to the
// Backend; this function owns only the orchestration.
async fn dispatch_tools<B: Backend>(
    assistant_msg: &Message,
    agent_tx: &mpsc::Sender<AgentEvent>,
    backend: &mut B,
) -> (Vec<ContentBlock>, bool) {
    let mut tool_results: Vec<ContentBlock> = Vec::new();
    let mut denied = false;

    for block in &assistant_msg.content {
        let ContentBlock::ToolUse { id, name, input } = block else {
            continue;
        };
        let summary = backend.tool_summary(name, input);
        let _ = agent_tx
            .send(AgentEvent::ToolUseStart {
                id: id.clone(),
                name: name.clone(),
                summary,
            })
            .await;

        if denied {
            // Cancel remaining tool calls but still return a tool_result for each —
            // the API requires one per tool_use_id in the prior assistant turn.
            // Mirror it as a UI event too, so the TUI's merged tool-call entry
            // resolves to an error state instead of showing "running" forever.
            let content =
                "Cancelled: a prior tool call in this turn was denied by the user.".to_string();
            let _ = agent_tx
                .send(AgentEvent::ToolResult {
                    id: id.clone(),
                    content: content.clone(),
                    is_error: true,
                })
                .await;
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content,
                is_error: true,
            });
            continue;
        }

        let allowed = if backend.requires_permission(name) {
            let (tx, rx) = oneshot::channel();
            if agent_tx
                .send(AgentEvent::PermissionRequest {
                    tool_use_id: id.clone(),
                    tool_name: name.clone(),
                    summary: backend.permission_summary(name, input),
                    respond: tx,
                })
                .await
                .is_err()
            {
                return (tool_results, true);
            }
            rx.await.unwrap_or(false)
        } else {
            true
        };

        let (content, is_error) = if allowed {
            match backend.execute(name, input, agent_tx).await {
                Ok(c) => (c, false),
                Err(e) => (format!("Tool execution error: {e:#}"), true),
            }
        } else {
            denied = true;
            ("User denied permission to run this tool.".into(), true)
        };

        let _ = agent_tx
            .send(AgentEvent::ToolResult {
                id: id.clone(),
                content: content.clone(),
                is_error,
            })
            .await;

        tool_results.push(ContentBlock::ToolResult {
            tool_use_id: id.clone(),
            content,
            is_error,
        });
    }

    (tool_results, denied)
}
