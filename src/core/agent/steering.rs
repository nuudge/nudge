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

// A supervised peer's check-in, decided by one inference in this agent's OWN loop:
// the check-in (plus the peer's capped activity digest) is appended to the real
// `messages` with an instruction to answer via `RespondToPeer`, and the exchange is
// recorded as it happened (check-in, the RespondToPeer call, its result, a one-line
// close) — which is both why the verdict is informed by full context and how the
// agent stays aware of its peer on later turns. The request is a normal turn (same
// tools, thinking on), so it shares the prompt cache. The verdict tool is requested
// in the prompt rather than forced via `tool_choice`: newer models reject forced
// tool choice outright.
//
// Verdicts: approve → allow; deny → block, and any `message` is delivered as the
// peer's next instruction (the peer paused on denial, so deny→redirect is one round
// trip); escalate → the request surfaces on this agent's own broker, named, and the
// human's answer is routed down. No verdict (provider failure, or none after
// VERDICT_ATTEMPTS) → escalate as well, with the check-in rolled back.
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
    tools.extend(peer_tools::schemas(peers, factory));
    let req = Request {
        model: &cfg.model,
        max_tokens: cfg.max_tokens,
        thinking_display: &cfg.thinking_display,
        system: backend.system_blocks(),
        tools,
        tool_cache_boundary,
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
                verdict = parse_verdict(&resp.content);
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
        return escalate_unjudged(
            session,
            messages,
            *last_good_snapshot,
            peers,
            agent_tx,
            &checkin,
            &name,
            &failure,
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

    // Answer the blocked peer first; a deny's redirect message rides right behind it
    // (the peer paused on denial and takes the message as its fresh instruction).
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

    // Record the exchange as it actually happened: the check-in (already pushed), the
    // model's real RespondToPeer call, its tool_result, and a one-line close that rests
    // the transcript on an assistant turn. Faithfulness is load-bearing here, not
    // tidiness: a check-in recorded as answered by plain text teaches the model, by
    // example, to answer the next check-in with plain text instead of the tool —
    // measured on a live transcript, 0/10 verdicts with a few text-only closes in
    // context vs 10/10 with the real call recorded. Session-logged for faithful resume.
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

// The model's RespondToPeer call, kept so the transcript can record it verbatim.
struct VerdictCall {
    id: String,
    input: Value,
}

struct Decision {
    call: VerdictCall,
    verdict: Verdict,
}

// An unknown verdict string counts as no verdict, same as a missing RespondToPeer.
fn parse_verdict(content: &[ContentBlock]) -> Option<Decision> {
    content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { id, name, input } if name == peer_tools::RESPOND_TO_PEER => {
            let message = input
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            let verdict = match input.get("verdict").and_then(Value::as_str) {
                Some("approve") => Verdict::Approve,
                Some("deny") => Verdict::Deny(message),
                Some("escalate") => Verdict::Escalate,
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

// Steering could not produce a verdict. Failing to decide is not a "no": denying here
// would hand the peer an unexplained refusal while this agent's transcript keeps no
// trace of it, so the call goes to the human exactly like an explicit escalate. The
// check-in is rolled back so the next real turn lands on a valid boundary.
#[allow(clippy::too_many_arguments)] // internal seam; mirrors run_steering_turn's state
async fn escalate_unjudged(
    session: &mut Session,
    messages: &mut Vec<Message>,
    last_good_snapshot: usize,
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    checkin: &CheckIn,
    name: &str,
    reason: &str,
) -> Result<()> {
    messages.truncate(last_good_snapshot);
    session.rollback();
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
