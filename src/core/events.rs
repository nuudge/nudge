use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

// Plain structs (not tuples) so the Kotlin mirror stays a data class.
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

// `supervised` is true when this session spawned the peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub kind: super::identity::ClientKind,
    pub supervised: bool,
    pub session_id: Option<String>,
    pub task: Option<String>,
}

// PermissionRequest's tool_use_id is unused (the prompt is modal, nothing to
// correlate) — hence the allow.
#[allow(dead_code)]
#[derive(Debug)]
pub enum AgentEvent {
    // Re-emitted on each turn boundary so controllers never detect cwd/branch locally.
    SessionInfo {
        model: String,
        cwd: String,
        git_branch: Option<String>,
        session_id: String,
        session_name: Option<String>,
        auto: bool,
    },
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
    // Logs can't go to stderr once the TUI owns the screen, so they ride back as this.
    Notice {
        text: String,
    },
    Error {
        message: String,
    },
}

// Clone and oneshot-free so the broker can buffer it for replay; the broker
// injects UserMessage/PermissionResolved so this stream alone rebuilds the transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControllerEvent {
    SessionInfo {
        model: String,
        cwd: String,
        git_branch: Option<String>,
        session_id: String,
        session_name: Option<String>,
        // Defaulted so a newer client still decodes frames from an older daemon.
        #[serde(default)]
        auto: bool,
    },
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
    PermissionRequest {
        tool_use_id: String,
        tool_name: String,
        summary: String,
    },
    // Lets replay render an answered prompt as history rather than re-prompting.
    PermissionResolved {
        tool_name: String,
        allow: bool,
    },
    ToolResult {
        id: String,
        content: String,
        is_error: bool,
    },
    // Broker-injected echo (the loop emits none); `sender` is stamped from the
    // sending controller's identity.
    UserMessage {
        text: String,
        sender: String,
    },
    TurnComplete,
    MaxIterations,
    Notice {
        text: String,
    },
    // Never emitted by the loop; only seeded from a resumed transcript.
    Warn {
        text: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum UiEvent {
    UserMessage { text: String },
    // Raw slash-command line, parsed server-side so clients don't reimplement the grammar.
    Command { line: String },
    // Fulfilled by the broker, which holds the loop's oneshot; never reaches the loop.
    PermissionResponse { tool_use_id: String, allow: bool },
    Quit,
}
