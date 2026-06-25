use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

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
    Usage {
        in_tokens: u64,
        out_tokens: u64,
        cache_write: u64,
        cache_read: u64,
    },
    AssistantText {
        text: String,
    },
    // Emitted for each non-empty `thinking` block in the assistant response.
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
    // emits no event for these.
    UserMessage {
        text: String,
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
// terminates them locally (PermissionResponse).
#[derive(Debug, Serialize, Deserialize)]
pub enum UiEvent {
    UserMessage { text: String },
    // Switch the API model. Applied at the next turn boundary — requests
    // already in flight finish on the old model.
    SetModel { model: String },
    // Rename the session. `name: Some` is the explicit name; `None` asks the loop
    // to derive one (git branch + short id in a repo, else an LLM-suggested
    // summary). Handled at a turn boundary, where it persists the label and
    // re-emits SessionInfo so every controller's header updates.
    RenameSession { name: Option<String> },
    // Connect a dormant MCP server by name (from the built-in catalog).
    LoadServer { name: String },
    // Disconnect a previously-loaded dormant server by name.
    UnloadServer { name: String },
    // Report loaded + available-dormant servers back as a Notice.
    ListServers,
    // Answer to a ControllerEvent::PermissionRequest, correlated by tool_use_id.
    // The broker holds the loop's oneshot Sender and fulfils it; this never
    // reaches the loop.
    PermissionResponse { tool_use_id: String, allow: bool },
    Quit,
}
