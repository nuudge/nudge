use crate::core::HandoffStatus;

// Stored semantically (not pre-rendered) so the Ctrl-O expand toggle can re-format.
pub(super) enum LogEntry {
    Blank,
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
    // Clamped to content height at render.
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
    // Input-box wrap width from the last render; Up/Down need it to map wrapped rows.
    pub(super) input_view_width: usize,
    // Empty = idle; non-empty drives the spinner.
    pub(super) status: String,
    pub(super) session_id: String,
    pub(super) session_name: Option<String>,
    pub(super) auto: bool,
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
    // (display label, API model id)
    pub(super) models: Vec<(String, String)>,
    pub(super) model_picker: Option<usize>,
    pub(super) mode: Mode,
    // Set by handlers, applied by the run loop (which holds the host handle).
    pub(super) pending_transition: Option<Mode>,
    // Present only on the owner TUI with NUDGE_RELAY set.
    pub(super) pairing_qr: Option<String>,
    pub(super) pairing_code: Option<String>,
    pub(super) pairing_qr_watch: Option<String>,
    pub(super) pairing_code_watch: Option<String>,
    pub(super) pairing_qr_agent: Option<String>,
    pub(super) pairing_code_agent: Option<String>,
    pub(super) pair_scope: PairScope,
    pub(super) handoff_status: Option<HandoffStatus>,
    // Cosmetic only: the broker is multi-attach, so a guest can't reclaim anything.
    pub(super) is_owner: bool,
    pub(super) self_name: String,
    pub(super) quit: bool,
}

// Header placeholders shown until the daemon's SessionInfo arrives and overwrites them.
pub struct UiConfig {
    pub session_id: String,
    pub session_name: Option<String>,
    pub auto: bool,
    pub model: String,
    pub thinking_display: String,
    pub pairing_qr: Option<String>,
    pub pairing_code: Option<String>,
    pub pairing_qr_watch: Option<String>,
    pub pairing_code_watch: Option<String>,
    pub pairing_qr_agent: Option<String>,
    pub pairing_code_agent: Option<String>,
    pub is_owner: bool,
    pub user_name: String,
    pub models: Vec<(String, String)>,
}

// Full is always minted when any pairing is; watch/agent may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PairScope {
    Full,
    Watch,
    Agent,
}

impl PairScope {
    pub(super) fn label(self) -> &'static str {
        match self {
            PairScope::Full => "full-access",
            PairScope::Watch => "watch-only",
            PairScope::Agent => "agent-peer",
        }
    }
}

impl App {
    pub(super) fn new(cfg: UiConfig) -> Self {
        Self {
            log: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_view_width: 0,
            status: String::new(),
            session_id: cfg.session_id,
            session_name: cfg.session_name,
            auto: cfg.auto,
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
            pairing_qr_watch: cfg.pairing_qr_watch,
            pairing_code_watch: cfg.pairing_code_watch,
            pairing_qr_agent: cfg.pairing_qr_agent,
            pairing_code_agent: cfg.pairing_code_agent,
            pair_scope: PairScope::Full,
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

    fn scope_qr(&self, scope: PairScope) -> Option<&String> {
        match scope {
            PairScope::Full => self.pairing_qr.as_ref(),
            PairScope::Watch => self.pairing_qr_watch.as_ref(),
            PairScope::Agent => self.pairing_qr_agent.as_ref(),
        }
    }

    pub(super) fn next_pairing_scope(&self) -> Option<PairScope> {
        const ORDER: [PairScope; 3] = [PairScope::Full, PairScope::Watch, PairScope::Agent];
        let cur = ORDER.iter().position(|s| *s == self.pair_scope).unwrap();
        (1..ORDER.len())
            .map(|step| ORDER[(cur + step) % ORDER.len()])
            .find(|s| self.scope_qr(*s).is_some())
    }

    pub(super) fn cycle_pairing(&mut self) {
        if let Some(next) = self.next_pairing_scope() {
            self.pair_scope = next;
        }
    }

    pub(super) fn visible_pairing_qr(&self) -> Option<&String> {
        self.scope_qr(self.pair_scope)
    }

    pub(super) fn visible_pairing_code(&self) -> Option<&String> {
        match self.pair_scope {
            PairScope::Full => self.pairing_code.as_ref(),
            PairScope::Watch => self.pairing_code_watch.as_ref(),
            PairScope::Agent => self.pairing_code_agent.as_ref(),
        }
    }
}
