use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::super::events::UiEvent;
use super::super::host::BrokerHandle;
use super::super::peer::{PeerFactory, PeerSet};
use crate::llm::SystemBlock;

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

// A model-facing listing of the peers currently held, so the agent knows which names
// it can address with MessagePeer without having to guess (and learn the roster from a
// failed call). `None` when peerless. The loop appends this as a TRAILING, uncached
// system block — after the volatile env breakpoint — so a peer connecting or leaving
// only reprocesses this small tail, never the cached stable prefix.
pub(super) fn roster_system_text(peers: &PeerSet) -> Option<String> {
    let infos = peers.infos();
    if infos.is_empty() {
        return None;
    }
    let mut out = String::from(
        "You are connected to these peer agents. Address one by its exact \
             name with the MessagePeer tool:",
    );
    for p in infos {
        let role = if p.supervised {
            "subagent you spawned"
        } else {
            "peer"
        };
        out.push_str(&format!("\n- {} ({role})", p.name));
    }
    Some(out)
}

// Spawning is autonomous token-spending and dismissal destroys a child's in-memory
// context (in-flight work stops) — the human gates both lifecycle actions. In auto
// mode nothing waits on the human: turning it on is their consent to delegation, and
// finished children must be dismissable unattended. Messaging is conversation (the
// addressee's own gates apply to whatever it causes), and a RespondToPeer verdict IS
// the supervision decision — gating it would just re-ask.
pub(super) fn requires_permission(name: &str, auto: bool) -> bool {
    !auto && (name == SPAWN || name == DISMISS_PEER)
}

// In auto mode the agent's own gated tools are refused instead of prompting the human.
// This is an error result, not a denial: a denial parks the loop waiting for human
// guidance, while an error lets the model read it and delegate in the same turn.
pub(super) fn auto_mode_refusal(name: &str) -> String {
    format!(
        "Auto mode is on: you supervise and do not run {name} yourself. Delegate the work \
         to a subagent with Spawn, or message an existing one with MessagePeer. Read, Grep \
         and Glob remain available for checking its work."
    )
}

// Trailing and uncached like the roster block, so toggling auto mode never busts the
// cached stable prefix.
pub(super) fn auto_mode_system_block() -> SystemBlock {
    SystemBlock {
        text: AUTO_MODE_TEXT.into(),
        cache: false,
    }
}

const AUTO_MODE_TEXT: &str = "## Auto mode is on

The user has stepped away and asked you to work hands-off: delegate the work to a subagent and supervise it. Nothing reaches the user until you report — no permission prompts, no escalations.

- Hand each task the user gives you to a subagent with Spawn. Make the task self-contained: the goal, the relevant context from this conversation, constraints, and what to report back. Then end your turn; the subagent's check-ins and report wake you.
- Supervise; don't do the work yourself. Your own Bash, Edit and CreateNew calls are refused in auto mode. Read, Grep and Glob are fine for checking the subagent's work.
- For a follow-up on the same work, message the existing subagent with MessagePeer instead of spawning a new one. If the user messages you while a subagent is working, relay what matters to it the same way.
- When the subagent reports, check the result and tell the user what was done. End with a \"Needs you\" list of every step that was skipped for the user to handle, or say there is none.

Deciding check-ins with RespondToPeer (approve or deny — there is no escalate in auto mode):
- approve: calls that serve the assigned task and stay within the working directory — edits, builds, tests, read-only commands, local git operations such as add, commit, and diff.
- deny, with a message saying what to do instead: off-task work, retrying an approach that already failed, anything against the project instructions.
- deny, with a message to skip the step, continue with the rest, and list it under \"Needs you\" in its report: deleting anything the task did not create, git push or history rewrites (reset --hard, rebase, force), writes outside the working directory, installing or upgrading packages, network access to hosts the task doesn't call for, and anything irreversible or that you are unsure about.";

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
pub(super) fn schemas(peers: &PeerSet, factory: &Option<PeerFactory>, auto: bool) -> Vec<Value> {
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
                assign follow-up work, ask a question, or report a result. Put everything \
                you have for one peer in a single message — consecutive messages to the same \
                peer are merged into one turn, so extra calls only cost tokens. Message a peer \
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
        // Auto mode drops escalate: nothing may wait on the absent human, so a risky
        // call is denied with a skip-and-report message instead.
        let (verdicts, escalate_text) = if auto {
            (json!(["approve", "deny"]), "")
        } else {
            (
                json!(["approve", "deny", "escalate"]),
                " 'escalate' hands the decision to your own user when you cannot judge \
                 it (irreversible actions, unclear intent, anything destructive).",
            )
        };
        out.push(json!({
            "name": RESPOND_TO_PEER,
            "description": format!("Deliver your verdict on a subagent's pending permission \
                check-in. Only meaningful while a check-in is being decided; calling it \
                at any other time is an error. Verdicts: \
                'approve' lets the subagent's tool call run; 'deny' blocks it — set \
                'message' to explain or redirect (it arrives as the subagent's next \
                instruction).{escalate_text}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "verdict": {
                        "type": "string",
                        "enum": verdicts
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
        // Dropping the Peer sends its owned SessionHost a final Quit: the child's
        // loop winds down cleanly and marks its session row ended; the transcript
        // persists in the store.
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
