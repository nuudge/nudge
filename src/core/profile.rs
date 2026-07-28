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
    // the session, does not receive supervision chatter, issues no commands, and may not
    // answer permission prompts. The last bit is what stops a child rubber-stamping its
    // parent's gated calls over the return edge (first-responder-wins would otherwise let
    // it beat the human); the supervisor edge uses `supervisor()` instead.
    pub fn agent_peer() -> Self {
        Self {
            may_quit: false,
            commands: CommandScope::None,
            receives_supervision: false,
            may_answer_permissions: false,
        }
    }

    // The edge from a spawner to the child it supervises: an agent peer that additionally
    // may answer the child's permission check-ins (that IS supervision — the parent
    // steers its child's gated calls). Assigned by direction at spawn time, which the
    // spawner alone knows; a peer never claims it.
    pub fn supervisor() -> Self {
        Self {
            may_answer_permissions: true,
            ..Self::agent_peer()
        }
    }
}
