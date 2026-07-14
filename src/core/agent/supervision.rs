use tokio::sync::mpsc;

use crate::core::events::{AgentEvent, ControllerEvent, UiEvent};
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

// Handle one event observed from a peer this agent drives. Step-3 supervision is
// deliberately minimal: activity surfaces to this agent's own front-end as a Notice
// (the watch substrate), and a permission check-in is auto-approved so the peer stays
// unblocked.
//
// PLACEHOLDER (step 5 replaces): the auto-approve becomes a real steering turn — the
// parent decides the peer's check-in with its full transcript in context (and gains
// escalation-to-human). For now it always allows, which is why the spawn path stays
// gated behind the hidden --spawn-demo flag.
pub(super) async fn supervise_peer_event(
    peers: &mut PeerSet,
    agent_tx: &mpsc::Sender<AgentEvent>,
    pid: PeerId,
    ev: Option<ControllerEvent>,
) {
    let name = peers.display_name(pid);
    match ev {
        None => {
            peers.remove(pid);
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("[peer {name}] disconnected"),
                })
                .await;
        }
        Some(ControllerEvent::PermissionRequest {
            tool_use_id,
            tool_name,
            summary,
        }) => {
            let _ = agent_tx
                .send(AgentEvent::Notice {
                    text: format!("[peer {name}] auto-approved {tool_name}: {summary}"),
                })
                .await;
            peers
                .drive(
                    pid,
                    UiEvent::PermissionResponse {
                        tool_use_id,
                        allow: true,
                    },
                )
                .await;
        }
        Some(other) => {
            if let Some(text) = peer_notice(&name, &other) {
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
        }
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
