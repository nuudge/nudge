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
pub(super) const RESPOND_TO_PEER: &str = "RespondToPeer";
pub(super) const DISMISS_PEER: &str = "DismissPeer";

pub(super) fn is_peer_tool(name: &str) -> bool {
    name == MESSAGE_PEER || name == SPAWN || name == RESPOND_TO_PEER || name == DISMISS_PEER
}

// Spawning is autonomous token-spending and dismissal destroys a child's in-memory
// context (in-flight work stops) — the human gates both lifecycle actions. Messaging
// is conversation (the addressee's own gates apply to whatever it causes), and a
// RespondToPeer verdict IS the supervision decision — gating it would just re-ask.
pub(super) fn requires_permission(name: &str) -> bool {
    name == SPAWN || name == DISMISS_PEER
}

pub(super) fn permission_summary(name: &str, input: &Value) -> String {
    match name {
        SPAWN => {
            let task = input.get("task").and_then(Value::as_str).unwrap_or("?");
            format!("spawn a subagent — task: {task}")
        }
        DISMISS_PEER => {
            let peer = input.get("peer").and_then(Value::as_str).unwrap_or("?");
            format!("dismiss subagent {peer} (ends its session; log persists)")
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
                still state in the task what you expect back and in what form. Its \
                messages and check-ins arrive on their own and wake you — never poll, \
                sleep, or busy-wait for it; simply end your turn. Use a subagent for \
                a self-contained task that can proceed in parallel; do not spawn one \
                for work you can do directly in a few steps.",
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
    if peers.has_supervised() {
        out.push(json!({
            "name": RESPOND_TO_PEER,
            "description": "Deliver your verdict on a subagent's pending permission \
                check-in. Only meaningful while a check-in is being decided (the call \
                is forced then); calling it at any other time is an error. Verdicts: \
                'approve' lets the subagent's tool call run; 'deny' blocks it — set \
                'message' to explain or redirect (it arrives as the subagent's next \
                instruction); 'escalate' hands the decision to your own user when you \
                cannot judge it (irreversible actions, unclear intent, anything \
                destructive).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "string",
                        "enum": ["approve", "deny", "escalate"]
                    },
                    "message": {
                        "type": "string",
                        "description": "For 'deny': the correction or redirect the \
                            subagent should follow instead. Ignored otherwise."
                    }
                },
                "required": ["verdict"]
            }
        }));
        out.push(json!({
            "name": DISMISS_PEER,
            "description": "End a subagent you spawned. Its in-memory session ends \
                (any in-flight work stops); its session log persists on disk and can \
                be resumed later. Use it when the subagent's task is done and you no \
                longer need it, or when it is stuck beyond redirection. Only \
                subagents you spawned can be dismissed.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "peer": {
                        "type": "string",
                        "description": "The subagent's name, e.g. 'child-ab12cd34'."
                    }
                },
                "required": ["peer"]
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
        RESPOND_TO_PEER => {
            let verdict = input.get("verdict").and_then(Value::as_str).unwrap_or("?");
            verdict.to_string()
        }
        DISMISS_PEER => {
            let peer = input.get("peer").and_then(Value::as_str).unwrap_or("?");
            format!("dismiss {peer}")
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
        // A RespondToPeer outside a steering turn has nothing to answer — the forced
        // call during steering is handled by `steering::run_steering_turn`, never here.
        RESPOND_TO_PEER => bail!("no pending peer check-in to respond to"),
        // Dropping the Peer drops its owned SessionHost: the child's broker forwards
        // a final Quit and the child ends cleanly; its JSONL persists on disk.
        DISMISS_PEER => {
            let Some(peer) = input.get("peer").and_then(Value::as_str) else {
                bail!("DismissPeer requires a 'peer' string");
            };
            let Some(id) = peers.find_by_name(peer) else {
                bail!(
                    "no peer named '{peer}'; current peers: {}",
                    peers.roster().join(", ")
                );
            };
            if !peers.is_supervised(id) {
                bail!("cannot dismiss '{peer}': not a subagent you spawned");
            }
            peers.remove(id);
            Ok(format!(
                "dismissed {peer}; its session ended (the session log persists on disk)"
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
