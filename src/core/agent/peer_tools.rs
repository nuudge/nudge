use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::super::events::UiEvent;
use super::super::host::BrokerHandle;
use super::super::peer::{PeerFactory, PeerSet};

// The loop-level peer tools. They live here — not on the `Backend` — because the
// `PeerSet` is the loop's own state: the backend stays peer-ignorant, and the loop
// appends these schemas to the request and intercepts their tool_use blocks before
// backend dispatch (see `dispatch_tools`).

pub(super) const MESSAGE_PEER: &str = "MessagePeer";
pub(super) const SPAWN: &str = "Spawn";

pub(super) fn is_peer_tool(name: &str) -> bool {
    name == MESSAGE_PEER || name == SPAWN
}

// Spawning is autonomous token-spending — and the current placeholder supervision
// auto-approves a child's gated calls — so the human gates every spawn. Messaging is
// conversation; the addressee's own gates apply to whatever it causes.
pub(super) fn requires_permission(name: &str) -> bool {
    name == SPAWN
}

pub(super) fn permission_summary(name: &str, input: &Value) -> String {
    match name {
        SPAWN => {
            let task = input.get("task").and_then(Value::as_str).unwrap_or("?");
            format!("spawn a subagent — task: {task}")
        }
        _ => summary(name, input),
    }
}

// Schemas the loop appends to the backend's tool array, per capability: `Spawn` only
// when a factory was injected (a top-level agent); `MessagePeer` whenever a peer is
// held or could be spawned. A factory-less, peerless agent advertises nothing.
// Placed after the backend's tools, so the stable cached prefix is never invalidated.
pub(super) fn schemas(peers: &PeerSet, factory: &Option<PeerFactory>) -> Vec<Value> {
    let mut out = Vec::new();
    if factory.is_some() {
        out.push(json!({
            "name": SPAWN,
            "description": "Spawn a subagent: a full peer agent in this directory with \
                the same built-in coding tools, started on the given task. The result \
                reports its name (e.g. 'child-ab12cd34') and session id — the spawn is \
                now part of your conversation, so you can refer to it later. The \
                subagent works autonomously; its activity surfaces to you as \
                '[peer …]' notices, and you can address it with MessagePeer. It has a \
                standing obligation to message you its result when done or blocked — \
                still state in the task what you expect back and in what form. Use a \
                subagent for a self-contained task that can proceed in parallel; do \
                not spawn one for work you can do directly in a few steps.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The subagent's assignment. Self-contained and \
                            specific: it starts with no context beyond this text."
                    }
                },
                "required": ["task"]
            }
        }));
    }
    if factory.is_some() || !peers.is_empty() {
        out.push(json!({
            "name": MESSAGE_PEER,
            "description": "Send a message to a peer agent you hold a connection to (a \
                subagent you spawned, or the agent that spawned you). The message arrives \
                as that agent's next instruction and triggers a turn on its side; use it to \
                assign follow-up work, ask a question, or report a result. Message a peer \
                only when it advances the task — never to acknowledge an acknowledgment or \
                exchange pleasantries: needless replies ping-pong between agents \
                indefinitely. Peer names appear in your transcript (e.g. in '[message from \
                peer …]' turns).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "peer": {
                        "type": "string",
                        "description": "The peer's name, e.g. 'child-ab12cd34'."
                    },
                    "message": {
                        "type": "string",
                        "description": "What to tell the peer. Self-contained: the peer sees \
                            your message, not your transcript."
                    }
                },
                "required": ["peer", "message"]
            }
        }));
    }
    out
}

// One-line collapsed-header summary (the loop can't ask the backend about a tool it
// doesn't know).
pub(super) fn summary(name: &str, input: &Value) -> String {
    match name {
        SPAWN => {
            let task = input.get("task").and_then(Value::as_str).unwrap_or("?");
            truncated(task, 80)
        }
        _ => {
            let peer = input.get("peer").and_then(Value::as_str).unwrap_or("?");
            let msg = input.get("message").and_then(Value::as_str).unwrap_or("");
            format!("to {peer}: {}", truncated(msg, 80))
        }
    }
}

fn truncated(s: &str, max: usize) -> String {
    let mut t: String = s.chars().take(max).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

// Execute a peer tool against the loop's own state.
//
// Spawn: the factory builds the child (and its return edge — it receives
// `self_handle` so the child can attach back to *this* agent), the loop registers it
// here, and the tool_result records name/id/task in the caller's transcript — which
// is exactly what makes the parent durably aware of its child.
//
// MessagePeer: resolve the addressee by name and drive its ui_tx — the exact path a
// human message takes into that agent, so it folds into the peer's context and
// triggers a turn with no extra machinery. Unknown name → error listing the current
// roster, so the model can self-correct.
pub(super) async fn execute(
    name: &str,
    input: &Value,
    peers: &mut PeerSet,
    factory: &Option<PeerFactory>,
    self_handle: &BrokerHandle,
) -> Result<String> {
    match name {
        SPAWN => {
            let Some(task) = input.get("task").and_then(Value::as_str) else {
                bail!("Spawn requires a 'task' string");
            };
            let Some(factory) = factory else {
                bail!("this agent cannot spawn subagents");
            };
            let reg = factory(task.to_string(), self_handle.clone()).await?;
            let peer_name = reg.who.name.clone();
            let session_id = reg.who.session_id.clone().unwrap_or_default();
            peers.register(reg);
            Ok(format!(
                "spawned peer {peer_name} (session {session_id}); it is now working on: {task}"
            ))
        }
        _ => {
            let Some(peer) = input.get("peer").and_then(Value::as_str) else {
                bail!("MessagePeer requires a 'peer' string");
            };
            let Some(message) = input.get("message").and_then(Value::as_str) else {
                bail!("MessagePeer requires a 'message' string");
            };
            let Some(id) = peers.find_by_name(peer) else {
                let roster = peers.roster();
                if roster.is_empty() {
                    bail!("no peer named '{peer}': you currently hold no peers");
                }
                bail!(
                    "no peer named '{peer}'; current peers: {}",
                    roster.join(", ")
                );
            };
            peers
                .drive(
                    id,
                    UiEvent::UserMessage {
                        text: message.to_string(),
                    },
                )
                .await;
            Ok(format!("message sent to {peer}"))
        }
    }
}
