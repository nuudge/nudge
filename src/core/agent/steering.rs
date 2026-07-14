use anyhow::Result;
use tokio::sync::{mpsc, oneshot};

use super::peer_tools;
use super::supervision::CheckIn;
use super::types::{AgentConfig, Backend};
use crate::core::events::{AgentEvent, UiEvent};
use crate::core::peer::{PeerFactory, PeerSet};
use crate::core::session::Session;
use crate::llm::{ContentBlock, Message, Provider, Request};

// A supervised peer's check-in, decided by one inference in this agent's OWN loop:
// the check-in (plus the peer's capped activity digest) is appended to the real
// `messages`, the model is forced onto `RespondToPeer` via `tool_choice`, and the
// exchange is recorded compactly (check-in + a one-line assistant close) — which is
// both why the verdict is informed by full context and how the agent stays aware of
// its peer on later turns. The request is byte-identical to a normal turn up to
// `tool_choice`, so it shares the prompt cache.
//
// Verdicts: approve → allow; deny → block, and any `message` is delivered as the
// peer's next instruction (the peer paused on denial, so deny→redirect is one round
// trip); escalate → the request surfaces on this agent's own broker, named, and the
// human's answer is routed down. Provider failure or a malformed verdict → safe
// deny, with the dangling check-in rolled back to `last_good_snapshot`.
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
    messages.push(Message {
        role: "user".into(),
        content: vec![ContentBlock::Text { text }],
    });
    session.log(messages.last().unwrap()).await?;

    let (mut tools, tool_cache_boundary) = backend.tool_schemas();
    tools.extend(peer_tools::schemas(peers, factory));
    let req = Request {
        model: &cfg.model,
        max_tokens: cfg.max_tokens,
        thinking_display: &cfg.thinking_display,
        system: backend.system_blocks(),
        tools,
        tool_cache_boundary,
        tool_choice: Some(peer_tools::RESPOND_TO_PEER),
        messages,
    };

    let resp = match provider.complete(&req).await {
        Ok(r) => r,
        Err(e) => {
            return safe_deny(
                messages,
                *last_good_snapshot,
                peers,
                agent_tx,
                &checkin,
                &name,
                &format!("steering inference failed: {e:#}"),
            )
            .await;
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

    // The forced call guarantees at most one RespondToPeer block; none = malformed.
    let verdict = resp.content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { name, input, .. } if name == peer_tools::RESPOND_TO_PEER => Some((
            input
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            input
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        )),
        _ => None,
    });
    let Some((verdict, message)) = verdict else {
        return safe_deny(
            messages,
            *last_good_snapshot,
            peers,
            agent_tx,
            &checkin,
            &name,
            "steering returned no verdict",
        )
        .await;
    };

    let (allow, closing) = match verdict.as_str() {
        "approve" => (
            true,
            format!("Approved {name}'s {} call.", checkin.tool_name),
        ),
        "deny" => (
            false,
            match &message {
                Some(m) => format!(
                    "Denied {name}'s {} call and redirected it: {m}",
                    checkin.tool_name
                ),
                None => format!("Denied {name}'s {} call.", checkin.tool_name),
            },
        ),
        "escalate" => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("escalating peer {name}'s {} call to you", checkin.tool_name),
                })
                .await;
            let (tx, rx) = oneshot::channel();
            let _ = agent_tx
                .send(AgentEvent::PermissionRequest {
                    tool_use_id: checkin.tool_use_id.clone(),
                    tool_name: checkin.tool_name.clone(),
                    summary: format!("peer {name} — {}", checkin.summary),
                    respond: tx,
                })
                .await;
            // A dropped prompt (front-end gone mid-escalation) is a deny.
            let allow = rx.await.unwrap_or(false);
            (
                allow,
                format!(
                    "Escalated {name}'s {} call to the user; they {} it.",
                    checkin.tool_name,
                    if allow { "allowed" } else { "denied" }
                ),
            )
        }
        other => (
            false,
            format!(
                "Denied {name}'s {} call (invalid steering verdict '{other}').",
                checkin.tool_name
            ),
        ),
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

    // Record the exchange as TWO entries — the check-in (already pushed) and a
    // synthetic assistant close carrying the decision — not the raw verdict
    // tool_use/tool_result pair, which is pure API-alternation ceremony. Supervision
    // bookkeeping is permanent context; at one check-in per gated peer call it would
    // otherwise dominate the transcript (observed live: 20 entries of scaffolding
    // around 1 entry of result). The close rests the transcript on an assistant turn
    // and is session-logged for faithful resume.
    messages.push(Message {
        role: "assistant".into(),
        content: vec![ContentBlock::Text {
            text: closing.clone(),
        }],
    });
    session.log(messages.last().unwrap()).await?;
    *last_good_snapshot = messages.len();

    let _ = agent_tx.send(AgentEvent::Notice { text: closing }).await;
    Ok(())
}

// Steering could not produce a verdict: never leave the peer hanging or the
// transcript dangling — deny (the peer pauses and can be redirected later) and roll
// the check-in turn back so the next real turn lands on a valid boundary.
async fn safe_deny(
    messages: &mut Vec<Message>,
    last_good_snapshot: usize,
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    checkin: &CheckIn,
    name: &str,
    reason: &str,
) -> Result<()> {
    messages.truncate(last_good_snapshot);
    peers
        .drive(
            checkin.pid,
            UiEvent::PermissionResponse {
                tool_use_id: checkin.tool_use_id.clone(),
                allow: false,
            },
        )
        .await;
    let _ = agent_tx
        .send(AgentEvent::Notice {
            text: format!("denied peer {name}'s {} call — {reason}", checkin.tool_name),
        })
        .await;
    Ok(())
}
