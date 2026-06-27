use tokio::sync::{mpsc, oneshot};

use crate::core::events::AgentEvent;
use crate::llm::{ContentBlock, Message};

use super::Backend;

// Orchestrate the assistant turn's tool calls: emit start/result events, gate on
// permission (one prompt at a time), short-circuit the batch after a denial, and
// return one ToolResult per call (the API requires it). Execution is the Backend's.
pub(super) async fn dispatch_tools<B: Backend>(
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
            // Still return a tool_result per cancelled call (the API requires one per
            // tool_use_id), and mirror it as an event so the TUI's entry leaves "running".
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
