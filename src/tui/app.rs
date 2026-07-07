use crate::core::HandoffStatus;

// Stored semantically (not pre-rendered) so the Ctrl-O expand toggle can re-format.
pub(super) enum LogEntry {
    Blank,
    // `sender` is the display name of whoever sent it; the renderer shows the local
    // user's own turns as "> " and others prefixed by their name.
    User {
        text: String,
        sender: String,
    },
    Assistant(String),
    Thinking(String),
    // result is filled in-place when the matching ToolResult arrives, correlated by id.
    ToolCall {
        id: String,
        name: String,
        summary: String,
        result: Option<ToolOutcome>,
    },
    Usage {
        in_tokens: u64,
        out_tokens: u64,
        cache_write: u64,
        cache_read: u64,
    },
    Allow(String),
    Deny(String),
    Info(String),
    Warn(String),
    Error(String),
}

pub(super) struct ToolOutcome {
    pub(super) content: String,
    pub(super) is_error: bool,
}

pub(super) struct PendingPermission {
    pub(super) tool_use_id: String,
    pub(super) tool_name: String,
    pub(super) summary: String,
    // Vertical scroll offset into the popup body; clamped to content height at render.
    pub(super) scroll: u16,
}

// Background = detached: the loop runs headless and buffers; the view freezes.
#[derive(PartialEq, Clone, Copy)]
pub(super) enum Mode {
    Foreground,
    Background,
}

pub(super) struct App {
    pub(super) log: Vec<LogEntry>,
    pub(super) input: String,
    // Char index (not byte); all edits go through the cursor helpers to stay in sync.
    pub(super) cursor: usize,
    // Empty = idle; non-empty drives the spinner.
    pub(super) status: String,
    pub(super) session_id: String,
    pub(super) session_name: Option<String>,
    pub(super) thinking_display: String,
    pub(super) model: String,
    pub(super) cwd_display: String,
    // Refreshed on TurnComplete — the agent may have run `git checkout`.
    pub(super) git_branch: Option<String>,
    pub(super) platform: String,
    pub(super) scroll: u16,
    pub(super) auto_scroll: bool,
    pub(super) expanded: bool,
    pub(super) spinner_frame: usize,
    pub(super) pending: Option<PendingPermission>,
    // (display label, API model id), fetched at startup where possible. Index
    // into this while the picker is open.
    pub(super) models: Vec<(String, String)>,
    pub(super) model_picker: Option<usize>,
    pub(super) mode: Mode,
    // Set by handlers, applied by the run loop (which holds the host handle).
    pub(super) pending_transition: Option<Mode>,
    // Present only on the owner TUI with NUDGE_RELAY set; drives the /background QR.
    pub(super) pairing_qr: Option<String>,
    pub(super) pairing_code: Option<String>,
    // Relay-dial progress; None until the first update.
    pub(super) handoff_status: Option<HandoffStatus>,
    // Cosmetic only: owner = this process hosts the loop (and may show a pairing QR);
    // guest = --connect. Drives the header badges in render.rs. There is no reclaim
    // capability — the broker is multi-attach, so clients coexist.
    pub(super) is_owner: bool,
    // This client's own display name, used to render its own user turns as "> "
    // (others show the sender's name). From the attach identity.
    pub(super) self_name: String,
    pub(super) quit: bool,
}

// Header placeholders shown until the daemon's SessionInfo arrives and overwrites them.
pub struct UiConfig {
    pub session_id: String,
    pub session_name: Option<String>,
    pub model: String,
    pub thinking_display: String,
    pub pairing_qr: Option<String>,
    pub pairing_code: Option<String>,
    pub is_owner: bool,
    pub user_name: String,
    pub models: Vec<(String, String)>,
}

impl App {
    pub(super) fn new(cfg: UiConfig) -> Self {
        Self {
            log: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: String::new(),
            session_id: cfg.session_id,
            session_name: cfg.session_name,
            thinking_display: cfg.thinking_display,
            model: cfg.model,
            cwd_display: String::new(),
            git_branch: None,
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            scroll: 0,
            auto_scroll: true,
            expanded: false,
            spinner_frame: 0,
            pending: None,
            models: cfg.models,
            model_picker: None,
            mode: Mode::Foreground,
            pending_transition: None,
            pairing_qr: cfg.pairing_qr,
            pairing_code: cfg.pairing_code,
            handoff_status: None,
            is_owner: cfg.is_owner,
            self_name: cfg.user_name,
            quit: false,
        }
    }

    pub(super) fn enter_background(&mut self) {
        self.mode = Mode::Background;
        self.status.clear();
        self.pending = None;
        self.model_picker = None;
    }

    // Clear the log so it rebuilds from the broker's replay on reattach.
    pub(super) fn enter_foreground(&mut self) {
        self.mode = Mode::Foreground;
        self.log.clear();
        self.scroll = 0;
        self.auto_scroll = true;
    }

    // Doesn't touch auto_scroll — an event shouldn't yank a reading user to the bottom.
    pub(super) fn push(&mut self, entry: LogEntry) {
        self.log.push(entry);
    }
}
