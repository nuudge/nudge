use tokio::sync::mpsc;

use crate::core::events::{AgentEvent, ControllerEvent};
use crate::core::identity::{ClientIdentity, ClientKind};
use crate::core::peer::{PeerId, PeerRegistration, PeerSet};

// Pends forever without a registrar, so the select arm stays quiet for a peerless session.
pub(super) async fn recv_registration(
    rx: &mut Option<mpsc::UnboundedReceiver<PeerRegistration>>,
) -> Option<PeerRegistration> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// Humans are named only in multi-driver sessions; agents always. Keep the prefix formats
// stable — the model learns to reference them. The session log stores the clean text.
pub(super) fn attribute(who: Option<&ClientIdentity>, text: String, multi: bool) -> String {
    match who {
        Some(w) if w.kind == ClientKind::Agent => {
            format!("[message from peer {}]\n{text}", w.name)
        }
        Some(w) if multi && !w.name.is_empty() => {
            format!("[message from {}]\n{text}", w.name)
        }
        _ => text,
    }
}

pub(super) fn attribute_message(
    who: Option<&ClientIdentity>,
    msg: &mut crate::llm::Message,
    multi: bool,
) {
    for block in &mut msg.content {
        if let crate::llm::ContentBlock::Text { text } = block {
            *text = attribute(who, std::mem::take(text), multi);
        }
    }
}

// A check-in is returned rather than handled: steering needs the whole loop state.
pub(super) enum Observed {
    Handled,
    CheckIn(CheckIn),
}

pub(super) struct CheckIn {
    pub pid: PeerId,
    pub tool_use_id: String,
    pub tool_name: String,
    pub summary: String,
}

// Only a supervised peer's permission requests are answered here; answering others would
// let e.g. a child rubber-stamp its parent's gated calls via first-responder-wins.
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
                Observed::Handled
            }
        }
        Some(other) => {
            if let Some(line) = activity_line(&other) {
                peers.record_activity(pid, &line);
            }
            // An unsupervised peer's activity belongs to its own session; narrating it
            // would flood this session's clients. Errors are rare and always surfaced.
            let narrate =
                peers.is_supervised(pid) || matches!(other, ControllerEvent::Error { .. });
            if narrate && let Some(text) = peer_notice(&name, &other) {
                let _ = agent_tx.send(AgentEvent::Notice { text }).await;
            }
            Observed::Handled
        }
    }
}

// Unprefixed: the check-in header already names the peer.
fn activity_line(ev: &ControllerEvent) -> Option<String> {
    match ev {
        ControllerEvent::AssistantText { text } => Some(format!("said: {text}")),
        // "requested", not "ran": ToolUseStart fires before the peer's permission gate,
        // so past tense would claim the very call being judged already executed.
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

// Text is clipped: a notice is a glance, the full report arrives via MessagePeer.
pub(super) fn peer_notice(name: &str, ev: &ControllerEvent) -> Option<String> {
    const NOTICE_CHARS_CAP: usize = 160;
    match ev {
        ControllerEvent::AssistantText { text } => {
            let mut t: String = text.chars().take(NOTICE_CHARS_CAP).collect();
            if t.chars().count() < text.chars().count() {
                t.push('…');
            }
            Some(format!("[peer {name}] {t}"))
        }
        ControllerEvent::ToolUseStart {
            name: tool,
            summary,
            ..
        } => Some(format!("[peer {name}] uses {tool}: {summary}")),
        ControllerEvent::PermissionResolved { tool_name, allow } => Some(format!(
            "[peer {name}] {} {tool_name}",
            if *allow { "allowed" } else { "denied" }
        )),
        ControllerEvent::Error { message } => Some(format!("[peer {name}] error: {message}")),
        _ => None,
    }
}
