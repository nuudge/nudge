use serde::{Deserialize, Serialize};

// Who is on one end of an attach. Announced by every client at attach time (humans
// and, later, peer agents alike) and recorded by the broker per controller, so a
// shared session can attribute messages and permission answers to a named party.
//
// Serialize/Deserialize: it rides in `wire::ClientFrame::Attach`, so it crosses the
// daemon socket and the relay. `session_id`/`task` are populated for a spawned agent
// (Phase 4); a human leaves them `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub kind: ClientKind,
    pub name: String,
    pub session_id: Option<String>,
    pub task: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKind {
    Human,
    Agent,
}

impl ClientIdentity {
    // A human client with no session/task context (the common case for a TUI).
    pub fn human(name: impl Into<String>) -> Self {
        Self {
            kind: ClientKind::Human,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }
}
