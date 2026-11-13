use anyhow::Result;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::peer_tools;
use super::supervision::CheckIn;
use super::types::{AgentConfig, Backend};
use crate::core::events::{AgentEvent, UiEvent};
use crate::core::peer::{PeerFactory, PeerSet};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request};

// A verdict-less reply is the model not following the instruction, which a fresh
// sample can fix; a provider error is not retried (it would fail the same way).
const VERDICT_ATTEMPTS: usize = 2;

const SKIP_AND_REPORT: &str = "Your supervisor did not approve that call. Skip that step, \
    continue with the rest of the task, and list the skipped step under \"Needs you\" in \
    your final report.";
const UNJUDGED_SKIP_AND_REPORT: &str = "Your supervisor could not judge that call. Skip \
    that step, continue with the rest of the task, and list the skipped step under \"Needs \
    you\" in your final report.";

// The check-in is decided by a normal turn in this agent's own `messages` (sharing the
// prompt cache) and recorded there. The verdict tool is asked for in the prompt, not
// forced via `tool_choice`: newer models reject forced tool choice.
// No verdict escalates to the human; in auto mode nothing may wait on the absent human,
// so escalate isn't offered and a deny always carries a message.
#[allow(clippy::too_many_arguments)] // internal seam; mirrors the loop's own state
pub(super) async fn run_steering_turn<P: Provider, B: Backend>(
    cfg: &AgentConfig,
    provider: &P,
    backend: &B,
    session: &mut Session,
    messages: &mut Vec<Message>,
    last_good_snapshot: &mut usize,
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    factory: &Option<PeerFactory>,
    auto: bool,
    checkin: CheckIn,
) -> Result<()> {
    let name = peers.display_name(checkin.pid);
    let digest = peers.drain_activity(checkin.pid);

    let mut text = format!(
        "[check-in from peer {name}] wants to run {}: {}\n",
        checkin.tool_name, checkin.summary
    );
    if digest.is_empty() {
        text.push_str("(no recorded activity since the last check-in)");
    } else {
        text.push_str("activity since the last check-in:");
        for line in &digest {
            text.push_str("\n- ");
            text.push_str(line);
        }
    }
    text.push_str(&format!(
        "\n\nDecide this check-in now by calling {} with your verdict; call no other tool.",
        peer_tools::RESPOND_TO_PEER
    ));
    messages.push(Message {
        role: "user".into(),
        content: vec![ContentBlock::Text { text }],
    });
    session.stage(messages.last().unwrap(), None);

    let (mut tools, tool_cache_boundary) = backend.tool_schemas();
    tools.extend(peer_tools::schemas(peers, factory, auto));
    let mut system = backend.system_blocks();
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
        container: None,
        messages,
    };

    let mut verdict = None;
    let mut failure = "steering returned no valid verdict".to_string();
    for _ in 0..VERDICT_ATTEMPTS {
        match provider.complete(&req).await {
            Ok(resp) => {
                let _ = agent_tx
                    .send(AgentEvent::Usage {
                        in_tokens: resp.usage.input_tokens,
                        out_tokens: resp.usage.output_tokens,
                        cache_write: resp.usage.cache_creation_input_tokens,
                        cache_read: resp.usage.cache_read_input_tokens,
                    })
                    .await;
                verdict = parse_verdict(&resp.content, auto);
                if verdict.is_some() {
                    break;
                }
            }
            Err(e) => {
                failure = format!("steering inference failed: {e:#}");
                break;
            }
        }
    }
    let Some(Decision { call, verdict }) = verdict else {
        return settle_unjudged(
            session,
            messages,
            *last_good_snapshot,
            peers,
            agent_tx,
            &checkin,
            &name,
            &failure,
            auto,
        )
        .await;
    };

    let (allow, message, closing) = match verdict {
        Verdict::Approve => (
            true,
            None,
            format!("Approved {name}'s {} call.", checkin.tool_name),
        ),
        Verdict::Deny(message) => {
            let message = message
                .filter(|m| !m.is_empty())
                .or_else(|| auto.then(|| SKIP_AND_REPORT.to_string()));
            let closing = match &message {
                Some(m) => format!(
                    "Denied {name}'s {} call and redirected it: {m}",
                    checkin.tool_name
                ),
                None => format!("Denied {name}'s {} call.", checkin.tool_name),
            };
            (false, message, closing)
        }
        Verdict::Escalate => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("escalating peer {name}'s {} call to you", checkin.tool_name),
                })
                .await;
            let allow = ask_human(agent_tx, &checkin, &name).await;
            (
                allow,
                None,
                format!(
                    "Escalated {name}'s {} call to the user; they {} it.",
                    checkin.tool_name,
                    if allow { "allowed" } else { "denied" }
                ),
            )
        }
    };

    // The peer paused on denial, so a redirect message sent right after becomes its
    // next instruction.
    peers
        .drive(
            checkin.pid,
            UiEvent::PermissionResponse {
                tool_use_id: checkin.tool_use_id,
                allow,
            },
        )
        .await;
    if !allow && let Some(m) = message.filter(|m| !m.is_empty()) {
        peers
            .drive(checkin.pid, UiEvent::UserMessage { text: m })
            .await;
    }

    // Record the real RespondToPeer call, not a plain-text answer: text-only closes in
    // context teach the model to skip the tool on the next check-in.
    let call_id = call.id.clone();
    for msg in [
        Message {
            role: "assistant".into(),
            content: vec![ContentBlock::ToolUse {
                id: call.id,
                name: peer_tools::RESPOND_TO_PEER.into(),
                input: call.input,
            }],
        },
        Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id,
                content: closing.clone(),
                is_error: false,
            }],
        },
        Message {
            role: "assistant".into(),
            content: vec![ContentBlock::Text {
                text: closing.clone(),
            }],
        },
    ] {
        messages.push(msg);
        session.stage(messages.last().unwrap(), None);
    }
    *last_good_snapshot = messages.len();
    session.commit()?;

    let _ = agent_tx.send(AgentEvent::Notice { text: closing }).await;
    Ok(())
}

enum Verdict {
    Approve,
    Deny(Option<String>),
    Escalate,
}

struct VerdictCall {
    id: String,
    input: Value,
}

struct Decision {
    call: VerdictCall,
    verdict: Verdict,
}

// An unknown verdict string counts as no verdict, same as a missing RespondToPeer. So
// does escalate in auto mode, where it is not offered.
fn parse_verdict(content: &[ContentBlock], auto: bool) -> Option<Decision> {
    content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { id, name, input } if name == peer_tools::RESPOND_TO_PEER => {
            let message = input
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            let verdict = match input.get("verdict").and_then(Value::as_str) {
                Some("approve") => Verdict::Approve,
                Some("deny") => Verdict::Deny(message),
                Some("escalate") if !auto => Verdict::Escalate,
                _ => return None,
            };
            Some(Decision {
                call: VerdictCall {
                    id: id.clone(),
                    input: input.clone(),
                },
                verdict,
            })
        }
        _ => None,
    })
}

// A dropped prompt (front-end gone mid-escalation) is a deny.
async fn ask_human(agent_tx: &mpsc::Sender<AgentEvent>, checkin: &CheckIn, name: &str) -> bool {
    let (tx, rx) = oneshot::channel();
    let _ = agent_tx
        .send(AgentEvent::PermissionRequest {
            tool_use_id: checkin.tool_use_id.clone(),
            tool_name: checkin.tool_name.clone(),
            summary: format!("peer {name} — {}", checkin.summary),
            respond: tx,
        })
        .await;
    rx.await.unwrap_or(false)
}

// Failing to decide is not a "no": a silent deny would leave no trace in this transcript,
// so it goes to the human (or, in auto mode, a skip-and-report deny). The check-in is
// rolled back so the next turn lands on a valid boundary.
#[allow(clippy::too_many_arguments)] // internal seam; mirrors run_steering_turn's state
async fn settle_unjudged(
    session: &mut Session,
    messages: &mut Vec<Message>,
    last_good_snapshot: usize,
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    checkin: &CheckIn,
    name: &str,
    reason: &str,
    auto: bool,
) -> Result<()> {
    messages.truncate(last_good_snapshot);
    session.rollback();
    if auto {
        let _ = agent_tx
            .send(AgentEvent::Notice {
                text: format!(
                    "couldn't decide peer {name}'s {} call ({reason}) — told it to skip the step and report it",
                    checkin.tool_name
                ),
            })
            .await;
        peers
            .drive(
                checkin.pid,
                UiEvent::PermissionResponse {
                    tool_use_id: checkin.tool_use_id.clone(),
                    allow: false,
                },
            )
            .await;
        peers
            .drive(
                checkin.pid,
                UiEvent::UserMessage {
                    text: UNJUDGED_SKIP_AND_REPORT.into(),
                },
            )
            .await;
        return Ok(());
    }
    let _ = agent_tx
        .send(AgentEvent::Notice {
            text: format!(
                "couldn't decide peer {name}'s {} call ({reason}) — escalating to you",
                checkin.tool_name
            ),
        })
        .await;
    let allow = ask_human(agent_tx, checkin, name).await;
    peers
        .drive(
            checkin.pid,
            UiEvent::PermissionResponse {
                tool_use_id: checkin.tool_use_id.clone(),
                allow,
            },
        )
        .await;
    Ok(())
}
