use serde::{Deserialize, Serialize};

// Attribution only (a claimable display name), not rights — see `ClientProfile`.
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
    pub fn human(name: impl Into<String>) -> Self {
        Self {
            kind: ClientKind::Human,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }
}
