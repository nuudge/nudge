use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

// The daemon's capability surface, carried in `Capabilities` so clients render
// pickers/menus from the daemon's data instead of a compiled-in list. Serialized
// as plain structs (not tuples) so the Kotlin mirror stays a data class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub usage: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub description: Option<String>,
    pub loaded: bool,
}

// One held peer edge, as advertised in Capabilities and reported by /peers.
// `supervised` is the direction-of-creation bit (true = this session spawned it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub kind: super::identity::ClientKind,
    pub supervised: bool,
    pub session_id: Option<String>,
    pub task: Option<String>,
}

// IDs are carried on every tool-related event so the UI can correlate updates:
// the TUI resolves a pending ToolCall entry in place when its ToolResult
// arrives. PermissionRequest's tool_use_id is still unused (the pending prompt
// is modal, so there's nothing to correlate yet) — hence the allow.
#[allow(dead_code)]
#[derive(Debug)]
pub enum AgentEvent {
    // Session context, re-emitted by the loop on each turn boundary (and on a model
    // switch) so every attached controller renders the daemon's current cwd/branch/
    // model/session without doing any local detection. The broker translates this to
    // the matching ControllerEvent. git_branch is None outside a repo.
    SessionInfo {
        model: String,
        cwd: String,
        git_branch: Option<String>,
        session_id: String,
        // The human label set via /session-rename, or None if the session is
        // still nameless. Controllers prefer it over `session_id` in the header.
        session_name: Option<String>,
    },
    // The daemon's capability surface (commands, models, MCP catalog, peer roster).
    // Seeded into the replay buffer next to SessionInfo and re-emitted when the
    // surface changes (an MCP load/unload, a peer spawned/dismissed/disconnected),
    // so every client renders menus from live daemon data.
    Capabilities {
        commands: Vec<CommandInfo>,
        models: Vec<ModelInfo>,
        mcp: Vec<McpServerInfo>,
        peers: Vec<PeerInfo>,
    },
    Usage {
        in_tokens: u64,
        out_tokens: u64,
        cache_write: u64,
        cache_read: u64,
    },
    AssistantText {
        text: String,
    },
    // Empty (when display: "omitted") and redacted_thinking blocks are skipped.
    AssistantThinking {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
        summary: String,
    },
    // The agent embeds a oneshot reply slot so it can `.await` a typed bool
    // instead of correlating an unrelated UiEvent response back to this request.
    PermissionRequest {
        tool_use_id: String,
        tool_name: String,
        summary: String,
        respond: oneshot::Sender<bool>,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    TurnComplete,
    MaxIterations,
    // A system-side message for the transcript (e.g. MCP load/unload outcomes).
    // Connect logs can't go to stderr once the TUI owns the screen, so they
    // ride back as an event the TUI renders as an info line.
    Notice {
        text: String,
    },
    Error {
        message: String,
    },
}

// The controller-facing event stream. Mirrors `AgentEvent` but is `Clone` and
// carries no `oneshot` — so the broker can buffer it for replay and fan it to
// whichever front-end is attached. The broker translates `AgentEvent` into this:
// it terminates `PermissionRequest`'s `oneshot` (keeping the `Sender` itself,
// keyed by `tool_use_id`) and injects `UserMessage` echoes + `PermissionResolved`
// markers so a controller reconstructs the whole transcript from this stream
// alone — live or on attach-replay.
//
// Serialize/Deserialize: this is the type that crosses the daemon socket (and,
// later, the relay) as the core→client half of the wire protocol; the framing
// lives in the `transport::wire` module (a layer above `core`). `AgentEvent`
// deliberately stays non-serializable — its `oneshot` is terminated at the broker
// before anything reaches the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerEvent {
    // Session context for a controller to render (e.g. the phone's header): model,
    // working directory, git branch, session id. Seeded into the replay buffer at
    // startup (so it's the first thing every attach sees) and re-emitted by the loop
    // on each turn boundary, so a daemon-side `git checkout` or `/model` switch
    // propagates to every client. git_branch is None outside a repo.
    SessionInfo {
        model: String,
        cwd: String,
        git_branch: Option<String>,
        session_id: String,
        // Human label set via /session-rename (None when nameless). Rendered in
        // place of the uuid in the header so a resumed session is recognizable.
        session_name: Option<String>,
    },
    // The daemon's capability surface — see AgentEvent::Capabilities. Seeded into
    // the replay buffer next to SessionInfo and re-emitted on surface change.
    Capabilities {
        commands: Vec<CommandInfo>,
        models: Vec<ModelInfo>,
        mcp: Vec<McpServerInfo>,
        peers: Vec<PeerInfo>,
    },
    Usage {
        in_tokens: u64,
        out_tokens: u64,
        cache_write: u64,
        cache_read: u64,
    },
    AssistantText {
        text: String,
    },
    AssistantThinking {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
        summary: String,
    },
    // Permission request without the oneshot; answer via UiEvent::PermissionResponse.
    PermissionRequest {
        tool_use_id: String,
        tool_name: String,
        summary: String,
    },
    // Resolution marker so replay renders an answered prompt as historical
    // (Allow/Deny line) rather than re-prompting. tool_name is carried so the
    // label can be rendered without remembering the original request.
    PermissionResolved {
        tool_name: String,
        allow: bool,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    // Echo of a submitted user message, so every controller (including one that
    // attaches later and replays) reconstructs the user's turns too — the loop
    // emits no event for these. `sender` is the display name of whoever sent it
    // (the broker stamps it from that controller's announced identity), so a shared
    // session can attribute each turn; a controller renders its own name as "you".
    UserMessage {
        text: String,
        sender: String,
    },
    TurnComplete,
    MaxIterations,
    Notice {
        text: String,
    },
    // A non-fatal warning rendered prominently (e.g. a truncated resume log).
    // The loop never emits this (so `translate` has no case for it); it is only
    // injected when seeding the buffer from a resumed transcript.
    Warn {
        text: String,
    },
    Error {
        message: String,
    },
}

// Serialize/Deserialize: the client→core half of the wire protocol, carried
// inside `wire::ClientFrame::Command`. The broker maps these to loop actions or
// terminates them locally (PermissionResponse / a gated Quit).
#[derive(Debug, Serialize, Deserialize)]
pub enum UiEvent {
    UserMessage { text: String },
    // A raw slash-command line (`/model …`, `/session-rename …`, `/mcp …`). The
    // loop parses and executes it server-side; results return as Notice/SessionInfo/
    // Capabilities events. One surface for every client — the grammar lives in one
    // place (`core::agent::command`), not re-implemented per front-end.
    Command { line: String },
    // Answer to a ControllerEvent::PermissionRequest, correlated by tool_use_id.
    // The broker holds the loop's oneshot Sender and fulfils it; this never
    // reaches the loop.
    PermissionResponse { tool_use_id: String, allow: bool },
    Quit,
}
