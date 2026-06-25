use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use tokio::sync::{mpsc, oneshot};

use super::events::{AgentEvent, UiEvent};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request, Response, SystemBlock};

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
    mut session: Session,
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

    // Last (model, git_branch, name) pushed as a SessionInfo, primed from the startup
    // values (which match main.rs's seed) so we only re-emit when one actually changes —
    // a `/model` switch, a mid-session `git checkout`, or a `/session-rename` — rather
    // than once per turn.
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
                            // Tier 3: no repo — ask the model for a short title from
                            // the conversation, falling back to the cwd's name. The
                            // `complete` future is `Send` (trait-guaranteed), so it's
                            // awaited inline here rather than via a `&P`-capturing
                            // helper, which would make the whole loop future non-Send.
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

            // Mirror current behavior: after any denial, pause for fresh user
            // guidance that rides along in the same tool_results turn so the
            // model sees "denied — try this instead" in one coherent step.
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

// Re-read the volatile context (model, git branch, name) and emit a SessionInfo only
// if it changed since the last emit, so every controller's header tracks the daemon
// without flooding the replay buffer with an identical event each turn. cwd/session-id
// are fixed for the session. git_branch() re-runs git, so call this at turn boundaries;
// the name changes only on /session-rename, which also routes through here. `branch` is
// read by the caller *before* the await (so no `&Backend` is held across it — that would
// force a `B: Sync` bound on the whole loop). model is borrowed; the rest are owned/Send.
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

// Persist a resolved session label and broadcast the change: write it to the index,
// emit a Notice (so the user — and the TUI, which can't know a daemon-derived name
// synchronously — sees the final label), then re-emit SessionInfo via the change-
// detecting helper, which now sees the new name and updates every controller's header.
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

// First 8 chars of the uuid — enough to disambiguate same-branch sessions in a label
// while staying short. Guarded against an unexpectedly short id.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

// Tier-3 fallback when the model can't suggest a title (empty conversation or API
// error): the working directory's own name, else a short id.
fn fallback_name(session: &Session) -> String {
    session
        .cwd
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(sanitize_title)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| short_id(&session.id).to_string())
}

// Build the tier-3 title request prompt from the conversation start. None when nothing
// has been said yet (a brand-new session renamed before the first turn) — the caller
// then falls back. The caller issues the actual `provider.complete` so the loop awaits
// the trait's `Send` future directly (a helper capturing `&P` would break `Send`).
fn title_prompt(messages: &[Message]) -> Option<String> {
    let digest = conversation_digest(messages)?;
    Some(format!(
        "Below is the start of a coding session.\n\n{digest}\n\nReply with ONLY a short, lowercase, kebab-case title (3-6 words joined by hyphens) describing the task. No quotes, no punctuation, no explanation."
    ))
}

// Extract a clean kebab-case title from the model's title response: first non-empty
// text block, normalized. None when the response carries no usable text.
fn title_from_response(resp: &Response) -> Option<String> {
    let raw = resp.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })?;
    let title = sanitize_title(raw);
    (!title.is_empty()).then_some(title)
}

// A bounded plain-text digest of the conversation start (user + assistant text only),
// capped so the title request stays cheap on long sessions. None when no text has been
// exchanged yet (a brand-new session renamed before the first turn).
fn conversation_digest(messages: &[Message]) -> Option<String> {
    const CAP: usize = 1500;
    let mut out = String::new();
    for m in messages {
        for block in &m.content {
            if let ContentBlock::Text { text } = block
                && !text.trim().is_empty()
            {
                out.push_str(text.trim());
                out.push('\n');
            }
        }
        if out.len() >= CAP {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(CAP).collect())
    }
}

// Normalize arbitrary text into a clean kebab-case label: lowercase alphanumerics,
// every other run collapsed to a single hyphen, trimmed of leading/trailing hyphens
// and capped in length. Used for both the model's suggestion and the cwd fallback.
fn sanitize_title(raw: &str) -> String {
    const MAX_LEN: usize = 60;
    let mut out = String::new();
    let mut pending_hyphen = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    // `out` is all ASCII (alphanumerics + hyphens), so a byte truncation is char-safe.
    // Trim any hyphen the cut left dangling.
    out.truncate(MAX_LEN);
    out.trim_end_matches('-').to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn sanitize_title_kebabs_and_trims() {
        assert_eq!(
            sanitize_title("Fix the Auth Retry Logic!"),
            "fix-the-auth-retry-logic"
        );
        // Leading/trailing/duplicate separators collapse to single hyphens.
        assert_eq!(sanitize_title("  --Hello,  World--  "), "hello-world");
        // Already-kebab input is preserved.
        assert_eq!(sanitize_title("add-user-login"), "add-user-login");
        // No alphanumerics → empty (caller falls back).
        assert_eq!(sanitize_title("!!! ??? ..."), "");
    }

    #[test]
    fn sanitize_title_caps_length() {
        let long = "word ".repeat(40);
        assert!(sanitize_title(&long).len() <= 60);
    }

    #[test]
    fn title_from_response_normalizes_first_text() {
        let resp = Response {
            content: vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::Text {
                    text: "  Add User Login Flow  ".into(),
                },
            ],
            stop_reason: "end_turn".into(),
            usage: Default::default(),
        };
        assert_eq!(
            title_from_response(&resp).as_deref(),
            Some("add-user-login-flow")
        );
    }

    #[test]
    fn title_prompt_none_without_text() {
        assert!(title_prompt(&[]).is_none());
        // A turn carrying only non-text content has nothing to summarize.
        let toolish = Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            }],
        };
        assert!(title_prompt(&[toolish]).is_none());
    }

    #[test]
    fn title_prompt_embeds_conversation_digest() {
        let prompt = title_prompt(&[user("Please refactor the parser")]).unwrap();
        assert!(prompt.contains("Please refactor the parser"));
        assert!(prompt.contains("kebab-case"));
    }
}
