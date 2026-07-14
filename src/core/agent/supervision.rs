use tokio::sync::mpsc;

use crate::core::events::{AgentEvent, ControllerEvent};
use crate::core::identity::{ClientIdentity, ClientKind};
use crate::core::peer::{PeerId, PeerRegistration, PeerSet};

// Await the next peer handed in at runtime; pend forever once the registrar is gone
// (or was never wired), so the loop's select arm stays quiet for a peerless session.
pub(super) async fn recv_registration(
    rx: &mut Option<mpsc::UnboundedReceiver<PeerRegistration>>,
) -> Option<PeerRegistration> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// Fold the broker-stamped sender into the text that enters the transcript: an agent
// peer's message is named, so the model knows which peer spoke; a human's message
// stays bare, exactly as it always has. Keep the prefix format stable — the model
// learns to reference it.
pub(super) fn attribute(who: Option<&ClientIdentity>, text: String) -> String {
    match who {
        Some(w) if w.kind == ClientKind::Agent => {
            format!("[message from peer {}]\n{text}", w.name)
        }
        _ => text,
    }
}

// The outcome of observing one peer event: most are fully handled here (Notices,
// reaping, digest recording); a supervised peer's permission check-in is returned to
// the loop, which runs a steering turn over its own transcript (`steering.rs`) —
// that needs the whole loop state, which this module deliberately doesn't hold.
pub(super) enum Observed {
    Handled,
    CheckIn(CheckIn),
}

// A supervised peer's pending permission request, awaiting this agent's verdict.
pub(super) struct CheckIn {
    pub pid: PeerId,
    pub tool_use_id: String,
    pub tool_name: String,
    pub summary: String,
}

// Handle one event observed from a peer this agent drives: activity surfaces to this
// agent's own front-end as a Notice (the watch substrate) and — for supervised peers
// — accrues in the capped digest ring for the next steering check-in. A permission
// request from a SUPERVISED peer becomes a `CheckIn` for the loop to steer; one from
// an unsupervised peer is never answered by this agent (its own supervisor or a
// human holds that decision — answering here would let e.g. a child rubber-stamp its
// parent's gated calls via first-responder-wins).
pub(super) async fn supervise_peer_event(
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    pid: PeerId,
    ev: Option<ControllerEvent>,
) -> Observed {
    let name = peers.display_name(pid);
    match ev {
        None => {
            peers.remove(pid);
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("[peer {name}] disconnected"),
                })
                .await;
            Observed::Handled
        }
        Some(ControllerEvent::PermissionRequest {
            tool_use_id,
            tool_name,
            summary,
        }) => {
            if peers.is_supervised(pid) {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: format!("[peer {name}] checks in — {tool_name}: {summary}"),
                    })
                    .await;
                Observed::CheckIn(CheckIn {
                    pid,
                    tool_use_id,
                    tool_name,
                    summary,
                })
            } else {
                let _ = agent_tx
                    .send(AgentEvent::Notice {
                        text: format!(
                            "[peer {name}] asks to use {tool_name}: {summary} (not mine to answer)"
                        ),
                    })
                    .await;
                Observed::Handled
            }
        }
        Some(other) => {
            if let Some(line) = activity_line(&other) {
                peers.record_activity(pid, &line);
            }
            if let Some(text) = peer_notice(&name, &other) {
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
            Observed::Handled
        }
    }
}

// One digest line per meaningful peer event, for the steering check-in. Unprefixed —
// the check-in header already names the peer; the ring in `PeerSet` enforces the
// line/length caps.
fn activity_line(ev: &ControllerEvent) -> Option<String> {
    match ev {
        ControllerEvent::AssistantText { text } => Some(format!("said: {text}")),
        // "requested", not "ran": ToolUseStart fires BEFORE the peer's permission
        // gate, and a gated call's own start-line always lands as the digest's last
        // entry for its own check-in — past tense would tell the steering model the
        // very call it is judging already executed. Completion is what the "-> ok" /
        // "-> error" result lines convey.
        ControllerEvent::ToolUseStart {
            name: tool,
            summary,
            ..
        } => Some(format!("requested {tool}: {summary}")),
        ControllerEvent::ToolResult {
            content, is_error, ..
        } => {
            if *is_error {
                let first = content.lines().next().unwrap_or("");
                Some(format!("-> error: {first}"))
            } else {
                Some("-> ok".to_string())
            }
        }
        _ => None,
    }
}

// Map a peer's observed event to a one-line Notice for this agent's front-end, or None
// for the noisy/internal events (usage, session info, turn markers) that add no watch
// value.
pub(super) fn peer_notice(name: &str, ev: &ControllerEvent) -> Option<String> {
    match ev {
        ControllerEvent::AssistantText { text } => Some(format!("[peer {name}] {text}")),
        ControllerEvent::ToolUseStart {
            name: tool,
            summary,
            ..
        } => Some(format!("[peer {name}] uses {tool}: {summary}")),
        ControllerEvent::PermissionResolved { tool_name, allow } => Some(format!(
            "[peer {name}] {} {tool_name}",
            if *allow { "allowed" } else { "denied" }
        )),
        // A peer's own Notices are deliberately NOT re-narrated. Under mutual attach I am
        // an attached controller of my peer, so a Notice I emit about it is fanned back
        // to it; if it re-narrated my Notices (and I its), one event would amplify into an
        // unbounded `[peer a] [peer b] [peer a] …` cascade. Every re-narration is itself a
        // Notice, so refusing to re-narrate Notices breaks the cycle at one hop. (The real
        // fix is identity-aware fan-out that never routes supervision chatter to peer
        // agents — deferred; see docs/symmetric-communication.md open questions.)
        ControllerEvent::Notice { .. } => None,
        ControllerEvent::Error { message } => Some(format!("[peer {name}] error: {message}")),
        _ => None,
    }
}
