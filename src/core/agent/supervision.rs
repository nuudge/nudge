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

// Fold the broker-stamped sender into the text that enters the transcript. An agent
// peer's message is always named, so the model knows which peer spoke. A human's
// message is named only in a multi-driver session (`multi`: two or more distinct
// driving senders have connected) — a solo session stays bare, exactly as it always
// has, while a shared session lets the model tell its drivers apart and address a
// reply. Keep the prefix formats stable — the model learns to reference them.
// Applied at payload-build time (live arrival and resume rebuild); the session log
// stores the clean text plus the sender (see `session::LoggedMessage`).
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

// Apply `attribute` to a logged user message's typed text blocks (tool_result
// blocks are untouched) — the model-facing form of a clean logged entry.
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
                // Not mine to answer (the peer's own human holds that decision), and not
                // mine to narrate either — an unsupervised peer's prompts are its own
                // session's business, same as the rest of its activity.
                Observed::Handled
            }
        }
        Some(other) => {
            if let Some(line) = activity_line(&other) {
                peers.record_activity(pid, &line);
            }
            // Activity narration only for supervised children — it's the parent's live
            // view of unattended work (no watch-mode for a child exists yet). An
            // unsupervised peer's activity belongs to its own session and human;
            // narrating it here floods this session's clients with another session's
            // work (10 chatty peers = 10 transcripts' worth). Its conversation still
            // arrives as MessagePeer turns, and errors are always surfaced (rare,
            // significant).
            let narrate =
                peers.is_supervised(pid) || matches!(other, ControllerEvent::Error { .. });
            if narrate && let Some(text) = peer_notice(&name, &other) {
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
// value. Assistant text is clipped — the notice is a watch-glance, not the record (a
// child's report arrives in full via MessagePeer).
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
