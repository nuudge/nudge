// Per-client rights at the broker boundary. The symmetry is in the mechanism (every
// client attaches the same way and speaks the same protocol); the differences are in
// policy — reified here as plain data, one profile per attached controller, read by
// the broker's routing (`deliver_to`) and verb gating (`verb_allowed`). The agent loop
// never sees a profile.
//
// This is deliberately NOT `ClientIdentity`: identity is attribution (a display name,
// claimable by the client); a profile is rights (assigned by provenance, never claimed).
// Keeping them apart is the design — a restricted human and a restricted agent that
// carry the same profile are indistinguishable to the broker.

// Which slash-commands a client may issue. `Only([...])` is intentionally omitted until
// a caller needs it — the two rules today are "all" and "none".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandScope {
    All,
    None,
}

#[derive(Debug, Clone)]
pub struct ClientProfile {
    // May end the session (UiEvent::Quit).
    pub may_quit: bool,
    // Which slash-commands (UiEvent::Command) the broker forwards to the loop, and
    // which appear in this client's Capabilities.
    pub commands: CommandScope,
    // Receives supervision chatter (Notice). False for a peer agent, which breaks the
    // mutual-attach amplification cascade at the boundary.
    pub receives_supervision: bool,
    // May answer a permission prompt (UiEvent::PermissionResponse). A watch-only client
    // observes prompts but cannot approve them.
    pub may_answer_permissions: bool,
}

impl ClientProfile {
    // A local human front-end: full rights.
    pub fn human() -> Self {
        Self {
            may_quit: true,
            commands: CommandScope::All,
            receives_supervision: true,
            may_answer_permissions: true,
        }
    }

    // A peer agent attached over a plain edge: it drives and observes, but does not end
    // the session, does not receive supervision chatter, and issues no commands. Whether
    // it may answer permissions is the one bit that varies by direction — the supervisor
    // edge (`supervisor()`) may; a bare peer edge may not.
    pub fn agent_peer() -> Self {
        Self {
            may_quit: false,
            commands: CommandScope::None,
            receives_supervision: false,
            may_answer_permissions: true,
        }
    }
}
