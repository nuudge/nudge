// Per-client rights at the broker boundary, assigned by provenance and never claimed
// (unlike `ClientIdentity`). The agent loop never sees a profile.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandScope {
    All,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientProfile {
    pub may_quit: bool,
    pub may_drive: bool,
    pub commands: CommandScope,
    // False for a peer agent, to break the mutual-attach amplification cascade.
    pub receives_supervision: bool,
    pub may_answer_permissions: bool,
}

impl ClientProfile {
    pub fn human() -> Self {
        Self {
            may_quit: true,
            may_drive: true,
            commands: CommandScope::All,
            receives_supervision: true,
            may_answer_permissions: true,
        }
    }

    // No permission answers: otherwise a child could rubber-stamp its parent's gated
    // calls over the return edge, beating the human under first-responder-wins.
    pub fn agent_peer() -> Self {
        Self {
            may_quit: false,
            may_drive: true,
            commands: CommandScope::None,
            receives_supervision: false,
            may_answer_permissions: false,
        }
    }

    // A `/connect-peer` edge: each side's own human gates its own session's permissions.
    pub fn agent() -> Self {
        Self {
            commands: CommandScope::All,
            ..Self::agent_peer()
        }
    }

    // Assigned by direction at spawn time, which only the spawner knows.
    pub fn supervisor() -> Self {
        Self {
            may_answer_permissions: true,
            ..Self::agent_peer()
        }
    }

    // An answerer that can't end the session: an agent approving another session's
    // prompts, as opposed to its human.
    pub fn is_supervisor(&self) -> bool {
        self.may_answer_permissions && !self.may_quit
    }

    pub fn watch_only() -> Self {
        Self {
            may_quit: false,
            may_drive: false,
            commands: CommandScope::None,
            receives_supervision: true,
            may_answer_permissions: false,
        }
    }
}
