use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::io::{self, Stdout, Write};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::coding::tools;
use crate::core::{Controller, ControllerEvent, HandoffStatus, SessionHandle, UiEvent};

mod markdown;

const EXPANDED_MAX_LINES: usize = 200;
const MAX_INPUT_LINES: usize = 8;
const SPINNER_TICK: Duration = Duration::from_millis(100);
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// Log entries are stored semantically so the renderer can re-format them when
// the user toggles ToolResult expansion. Pre-rendering to Lines (the earlier
// design) made expand/collapse impossible without re-running every event.
enum LogEntry {
    Blank,
    User(String),
    Assistant(String),
    Thinking(String),
    // One entry per tool call: created on ToolUseStart with result=None
    // (spinner bullet, "running…"), then resolved *in place* when the matching
    // ToolResult arrives — correlated by `id`. Keeping use+result together is
    // what lets the collapsed view show one compact action group per tool.
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

struct ToolOutcome {
    content: String,
    is_error: bool,
}

struct PendingPermission {
    tool_use_id: String,
    tool_name: String,
    summary: String,
}

// Foreground = attached to the session (drives + observes). Background = detached
// (the loop runs headless and buffers; the TUI shows a frozen banner). Reattach
// replays the buffer.
#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Foreground,
    Background,
}

struct App {
    log: Vec<LogEntry>,
    input: String,
    // Char index of the edit cursor within `input` (0..=char count). All input
    // mutations go through the cursor helpers so insert/delete and arrow/Ctrl-A/E
    // movement stay consistent with the rendered cursor position.
    cursor: usize,
    // Empty when the agent is idle. Non-empty drives the spinner — the agent
    // sets it via events (ToolUseStart, ToolResult) and the UI sets it on
    // user input submit. TurnComplete / Error clear it.
    status: String,
    // The id the Session was created with — the same value the user passes
    // to --resume. Handed in at startup, not re-derived from artifacts.
    session_id: String,
    // The human label set via /session-rename, None when the session is nameless.
    // Shown in place of the uuid in the header. Updated by SessionInfo events.
    session_name: Option<String>,
    // The thinking display mode the agent was launched with. Shown in the
    // status line so the user can't forget which mode is active — particularly
    // useful when "omitted" is set and no thinking entries appear.
    thinking_display: String,
    model: String,
    cwd_display: String,
    // Current git branch, None when cwd isn't inside a repo. Refreshed on
    // TurnComplete — the agent itself may have run `git checkout`.
    git_branch: Option<String>,
    platform: String,
    scroll: u16,
    auto_scroll: bool,
    expanded: bool,
    spinner_frame: usize,
    pending: Option<PendingPermission>,
    // Some(selected index into crate::MODELS) while the /model picker is open.
    model_picker: Option<usize>,
    // Foreground/Background; Background freezes the view (detached from the loop).
    mode: Mode,
    // A requested mode change, set by handlers and executed by the run loop
    // (which owns the SessionHost handle needed to attach/detach).
    pending_transition: Option<Mode>,
    // The scannable pairing QR + paste-able code, present only when NUDGE_RELAY is
    // set (the owner TUI). When set, /background renders the QR in-screen above the
    // banner so a phone can attach; when None, /background is the plain frozen banner.
    pairing_qr: Option<String>,
    pairing_code: Option<String>,
    // Progress of the relay dial that arms phone handoff, streamed from the dial task
    // on /background. `None` until the first update arrives (rendered as "connecting").
    // Only meaningful for the owner TUI with a relay configured.
    handoff_status: Option<HandoffStatus>,
    // True for the in-process owner TUI (hosts the agent loop; can force-reclaim a
    // session a guest holds; quitting ends it). False for a `--connect` guest
    // (local socket or remote relay). Drives a header badge + the foreground
    // failure message — purely cosmetic; the *capability* lives in `attach_force`.
    is_owner: bool,
    quit: bool,
}

// Initial placeholders for the header. cwd / git branch / model / session id are all
// authoritatively supplied by the daemon's `SessionInfo` event (seeded first on attach
// and re-emitted on change), so these are only what shows for the brief moment before
// that first event arrives. thinking_display is purely a client-side launch flag.
pub struct UiConfig {
    pub session_id: String,
    pub session_name: Option<String>,
    pub model: String,
    pub thinking_display: String,
    // Set only when launched with `--relay`: the scannable pairing QR and the
    // paste-able `nudge:` code, shown on /background so a phone can attach. `None`
    // for a plain local session (then /background is the in-screen frozen banner).
    pub pairing_qr: Option<String>,
    pub pairing_code: Option<String>,
    // True for the in-process owner TUI; false for a `--connect` guest. Drives the
    // header badge and the foreground-failure wording.
    pub is_owner: bool,
}

impl App {
    fn new(cfg: UiConfig) -> Self {
        Self {
            log: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: String::new(),
            session_id: cfg.session_id,
            session_name: cfg.session_name,
            thinking_display: cfg.thinking_display,
            model: cfg.model,
            // Filled by the daemon's SessionInfo (first event on attach); blank until then.
            cwd_display: String::new(),
            git_branch: None,
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
            scroll: 0,
            auto_scroll: true,
            expanded: false,
            spinner_frame: 0,
            pending: None,
            model_picker: None,
            mode: Mode::Foreground,
            pending_transition: None,
            pairing_qr: cfg.pairing_qr,
            pairing_code: cfg.pairing_code,
            handoff_status: None,
            is_owner: cfg.is_owner,
            quit: false,
        }
    }

    // Detached: freeze the view and dismiss any modal — we're no longer driving.
    fn enter_background(&mut self) {
        self.mode = Mode::Background;
        self.status.clear();
        self.pending = None;
        self.model_picker = None;
    }

    // Reattached: clear the log so the caller can rebuild it (resume backlog via
    // seed_replay, then the broker's buffer replay streaming in as events).
    fn enter_foreground(&mut self) {
        self.mode = Mode::Foreground;
        self.log.clear();
        self.scroll = 0;
        self.auto_scroll = true;
    }

    // While backgrounded only two keys act: Enter to foreground, Ctrl-C to quit.
    fn handle_background_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.pending_transition = Some(Mode::Foreground),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            _ => {}
        }
    }

    // Agent events use this without touching auto_scroll — if the user is
    // reading history, an event arriving shouldn't yank them back to the
    // bottom. User-initiated actions (submit, permission answer) set
    // auto_scroll = true explicitly at their call sites.
    fn push(&mut self, entry: LogEntry) {
        self.log.push(entry);
    }

    fn handle_agent_event(&mut self, event: ControllerEvent) {
        match event {
            ControllerEvent::Usage {
                in_tokens,
                out_tokens,
                cache_write,
                cache_read,
            } => {
                self.push(LogEntry::Usage {
                    in_tokens,
                    out_tokens,
                    cache_write,
                    cache_read,
                });
            }
            ControllerEvent::AssistantText { text } => {
                self.push(LogEntry::Assistant(text));
                self.push(LogEntry::Blank);
            }
            ControllerEvent::AssistantThinking { text } => {
                self.push(LogEntry::Thinking(text));
                self.push(LogEntry::Blank);
            }
            ControllerEvent::ToolUseStart { id, name, summary } => {
                self.status = format!("running {name}");
                self.push(LogEntry::ToolCall {
                    id,
                    name,
                    summary,
                    result: None,
                });
            }
            ControllerEvent::PermissionRequest {
                tool_use_id,
                tool_name,
                summary,
            } => {
                self.pending = Some(PendingPermission {
                    tool_use_id,
                    tool_name,
                    summary,
                });
            }
            ControllerEvent::PermissionResolved { tool_name, allow } => {
                // Clear the modal: in the live path the keypress already took
                // `pending`; on replay there was no keypress, so clear it here so
                // an answered prompt doesn't reappear as actionable.
                self.pending = None;
                self.push(if allow {
                    LogEntry::Allow(tool_name)
                } else {
                    LogEntry::Deny(tool_name)
                });
                self.auto_scroll = true;
            }
            ControllerEvent::UserMessage { text } => {
                // Rendered from the broker's echo (not optimistically on submit),
                // so one path serves both live input and attach-replay.
                self.push(LogEntry::User(text));
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let outcome = ToolOutcome { content, is_error };
                // Search from the back: the matching ToolCall is almost always
                // the most recent entry, and rposition keeps duplicate ids (not
                // expected, but cheap to be correct about) resolving newest-first.
                let idx = self.log.iter().rposition(
                    |e| matches!(e, LogEntry::ToolCall { id: eid, result: None, .. } if *eid == id),
                );
                match idx {
                    Some(i) => {
                        if let LogEntry::ToolCall { result, .. } = &mut self.log[i] {
                            *result = Some(outcome);
                        }
                    }
                    None => {
                        // No pending ToolCall (shouldn't happen) — still show
                        // the result rather than dropping it.
                        self.push(LogEntry::ToolCall {
                            id,
                            name: "(unknown tool)".into(),
                            summary: String::new(),
                            result: Some(outcome),
                        });
                    }
                }
                self.push(LogEntry::Blank);
                // Tool finished — agent will now make the next API call.
                self.status = "thinking".into();
            }
            ControllerEvent::TurnComplete => {
                // Branch changes (a mid-session `git checkout`) arrive via the daemon's
                // SessionInfo, re-emitted on each turn boundary — no local git here.
                self.status.clear();
            }
            ControllerEvent::MaxIterations => {
                self.push(LogEntry::Warn("hit MAX_ITERATIONS".into()));
                self.status.clear();
            }
            ControllerEvent::Notice { text } => {
                // One Info row per line — Info renders single-line (newlines get
                // flattened to spaces), so a multi-line notice (the /mcp list)
                // must be split to stay readable.
                for line in text.lines() {
                    self.push(LogEntry::Info(line.to_string()));
                }
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::Warn { text } => {
                self.push(LogEntry::Warn(text));
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
            ControllerEvent::Error { message } => {
                self.push(LogEntry::Error(message));
                self.status.clear();
            }
            // Adopt the daemon's context as the header. Replayed first on attach, so a
            // remote (`--connect`) TUI shows the daemon's cwd/branch/model/session id
            // rather than its own local environment. For a local in-process session the
            // values already match what `App::new` derived, so this is idempotent.
            ControllerEvent::SessionInfo {
                model,
                cwd,
                git_branch,
                session_id,
                session_name,
            } => {
                self.model = model;
                self.cwd_display = cwd;
                self.git_branch = git_branch;
                self.session_id = session_id;
                self.session_name = session_name;
            }
        }
    }

    // Byte offset into `input` for the current char cursor. Clamps to the end
    // so an out-of-range cursor can't panic on insert/remove.
    fn cursor_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    fn insert_char(&mut self, c: char) {
        let b = self.cursor_byte();
        self.input.insert(b, c);
        self.cursor += 1;
    }

    // Delete the char before the cursor (Backspace).
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let b = self.cursor_byte();
        self.input.remove(b);
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    // Char index of the start of the line the cursor is on (just after the
    // preceding '\n', or 0). Powers Ctrl-A.
    fn line_start(&self) -> usize {
        let mut start = 0;
        for (i, c) in self.input.chars().take(self.cursor).enumerate() {
            if c == '\n' {
                start = i + 1;
            }
        }
        start
    }

    // Char index of the end of the line the cursor is on (at the next '\n', or
    // the buffer end). Powers Ctrl-E.
    fn line_end(&self) -> usize {
        self.input
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, c)| *c == '\n')
            .map(|(i, _)| i)
            .unwrap_or_else(|| self.input.chars().count())
    }

    fn handle_key(&mut self, key: KeyEvent, ui_tx: &mpsc::Sender<UiEvent>) {
        // With DISAMBIGUATE_ESCAPE_CODES the terminal also reports Release /
        // Repeat events — ignore them so we don't double-fire on every press.
        if key.kind != KeyEventKind::Press {
            return;
        }
        if let Some(pending) = self.pending.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = ui_tx.try_send(UiEvent::PermissionResponse {
                        tool_use_id: pending.tool_use_id,
                        allow: true,
                    });
                    self.auto_scroll = true;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    let _ = ui_tx.try_send(UiEvent::PermissionResponse {
                        tool_use_id: pending.tool_use_id,
                        allow: false,
                    });
                    self.auto_scroll = true;
                }
                _ => {
                    self.pending = Some(pending);
                }
            }
            return;
        }
        if let Some(sel) = self.model_picker {
            match key.code {
                KeyCode::Up => self.model_picker = Some(sel.saturating_sub(1)),
                KeyCode::Down => {
                    self.model_picker = Some((sel + 1).min(crate::MODELS.len() - 1));
                }
                KeyCode::Enter => {
                    self.model_picker = None;
                    let (label, id) = crate::MODELS[sel];
                    if id != self.model {
                        self.model = id.to_string();
                        self.push(LogEntry::Info(format!("model set to {label} ({id})")));
                        self.push(LogEntry::Blank);
                        self.auto_scroll = true;
                        let _ = ui_tx.try_send(UiEvent::SetModel {
                            model: id.to_string(),
                        });
                    }
                }
                KeyCode::Esc => self.model_picker = None,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _ = ui_tx.try_send(UiEvent::Quit);
                self.quit = true;
            }
            KeyCode::Char('d')
                if key.modifiers.contains(KeyModifiers::CONTROL) && self.input.is_empty() =>
            {
                let _ = ui_tx.try_send(UiEvent::Quit);
                self.quit = true;
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Toggle preserves scroll position — render will clamp if
                // max_scroll shrank, and re-engage auto-follow if scroll
                // happens to land at the new bottom.
                self.expanded = !self.expanded;
            }
            // Alt+Enter or Ctrl+Enter inserts a newline. Shift+Enter is
            // intentionally not supported: it requires the kitty protocol's
            // REPORT_ALL_KEYS_AS_ESCAPE_CODES flag to disambiguate from plain
            // Enter, which currently breaks IME composition in crossterm 0.28
            // (no REPORT_ASSOCIATED_TEXT support yet). Ctrl+J also works as a
            // wire-unambiguous fallback handled below.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.insert_char('\n');
            }
            // Ctrl+J fallback: many terminals (incl. Alacritty without the
            // kitty keyboard protocol negotiated) cannot distinguish Ctrl+Enter
            // from plain Enter on the wire — both arrive as CR. But Ctrl+J
            // (LF, 0x0A) is unambiguous and is what those terminals send when
            // the user expects "Ctrl+Enter". Accept it as a newline so the
            // chord works even on legacy keyboard-protocol terminals.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char('\n');
            }
            // Backslash-continuation: an unescaped trailing `\` before Enter
            // means "newline, don't submit". Shell convention — works on
            // every terminal regardless of modifier-reporting quirks, so
            // it's the always-available escape hatch. Trailing-backslash
            // parity follows POSIX shells: odd count → continuation (strip
            // one, append newline); even count → literal pairs, submit.
            KeyCode::Enter
                if self
                    .input
                    .chars()
                    .take(self.cursor)
                    .fold(0usize, |run, c| if c == '\\' { run + 1 } else { 0 })
                    % 2
                    == 1 =>
            {
                self.backspace();
                self.insert_char('\n');
            }
            KeyCode::Enter => {
                let trimmed = self.input.trim();
                if trimmed.is_empty() {
                    return;
                }
                // Single-line input starting with `/` is a slash command, not
                // a message — multi-line input that happens to start with `/`
                // (e.g. a pasted path) still goes to the model.
                if trimmed.starts_with('/') && !trimmed.contains('\n') {
                    let cmd = trimmed.to_string();
                    self.clear_input();
                    self.handle_command(&cmd, ui_tx);
                    return;
                }
                let text = std::mem::take(&mut self.input);
                self.cursor = 0;
                // The user turn is rendered when its echo returns from the broker,
                // not optimistically here — keeps live + replay rendering identical.
                self.status = "thinking".into();
                self.auto_scroll = true;
                let _ = ui_tx.try_send(UiEvent::UserMessage { text });
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_start();
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = self.line_end();
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace => {
                self.backspace();
            }
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                // auto_scroll re-engages in render() if this lands at bottom.
            }
            KeyCode::Home => {
                self.auto_scroll = false;
                self.scroll = 0;
            }
            KeyCode::End => {
                // Re-engage tail-follow; render clamps scroll to max.
                self.auto_scroll = true;
            }
            _ => {}
        }
    }

    fn handle_command(&mut self, cmd: &str, ui_tx: &mpsc::Sender<UiEvent>) {
        let mut parts = cmd.split_whitespace();
        match parts.next() {
            Some("/model") => {
                let current = crate::MODELS
                    .iter()
                    .position(|(_, id)| *id == self.model)
                    .unwrap_or(0);
                self.model_picker = Some(current);
            }
            // /mcp [load|unload <name>] — list, connect, or disconnect dormant
            // servers. The registry lives in the agent task, so results come
            // back asynchronously as a Notice rather than inline here.
            Some("/mcp") => {
                let event = match (parts.next(), parts.next()) {
                    (None, _) => Some(UiEvent::ListServers),
                    (Some("load"), Some(name)) => Some(UiEvent::LoadServer {
                        name: name.to_string(),
                    }),
                    (Some("unload"), Some(name)) => Some(UiEvent::UnloadServer {
                        name: name.to_string(),
                    }),
                    _ => {
                        self.push(LogEntry::Warn(
                            "usage: /mcp | /mcp load <name> | /mcp unload <name>".into(),
                        ));
                        self.push(LogEntry::Blank);
                        self.auto_scroll = true;
                        None
                    }
                };
                if let Some(event) = event {
                    let _ = ui_tx.try_send(event);
                }
            }
            // Rename the session. With an argument it's the explicit name; bare, the
            // daemon derives one (git branch + short id in a repo, else an LLM-suggested
            // summary). The final label arrives back as a Notice + SessionInfo, so the
            // header updates without the TUI guessing the derived name locally.
            Some("/session-rename") => {
                let arg = cmd["/session-rename".len()..].trim();
                let name = (!arg.is_empty()).then(|| arg.to_string());
                let _ = ui_tx.try_send(UiEvent::RenameSession { name });
            }
            // Detach and run the agent headless; the run loop performs the actual
            // detach (it holds the SessionHost). Reattach with Enter.
            Some("/background") | Some("/bg") => {
                self.pending_transition = Some(Mode::Background);
            }
            _ => {
                self.push(LogEntry::Warn(format!("unknown command: {cmd}")));
                self.push(LogEntry::Blank);
                self.auto_scroll = true;
            }
        }
    }

    fn handle_mouse(&mut self, ev: MouseEvent) -> bool {
        // Returns true when the event changed visible state so the caller
        // can skip the redraw for mouse motion noise.
        match ev.kind {
            MouseEventKind::ScrollUp => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                true
            }
            _ => false,
        }
    }

    fn handle_paste(&mut self, text: String) {
        // Bracketed paste delivers the whole clipboard as one event, so
        // embedded newlines stay intact instead of triggering submit.
        // Normalize tabs / control chars (except newline) at the boundary:
        // the input Paragraph renders the raw string, and a literal tab both
        // garbles terminal cells and desyncs the char-count cursor math.
        for c in text.chars() {
            match c {
                '\t' => {
                    for _ in 0..4 {
                        self.insert_char(' ');
                    }
                }
                '\n' => self.insert_char('\n'),
                c if c.is_control() => {}
                c => self.insert_char(c),
            }
        }
    }

    // `width` is the log viewport's inner width — collapsed action lines are
    // truncated to it so they never wrap into multi-row noise.
    fn render_log(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for entry in &self.log {
            match entry {
                LogEntry::Blank => out.push(Line::from("")),
                LogEntry::User(text) => emit_prefixed(
                    &mut out,
                    text,
                    "> ",
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                LogEntry::Assistant(text) => {
                    let body = markdown::render(text);
                    emit_prefixed_lines(&mut out, body, "* ", Style::default().fg(Color::Cyan))
                }
                LogEntry::Thinking(text) => {
                    // Italicized, dim magenta — kept off the assistant/heading
                    // cyan so a thinking block never reads as model output.
                    // Behaves like tool results under the global `expanded`
                    // toggle (Ctrl-O): quiet by default, inspectable on demand.
                    let style = Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::ITALIC | Modifier::DIM);
                    if self.expanded {
                        emit_expanded_thinking(&mut out, text, style);
                    } else {
                        let marker = "✻ Thinking… ";
                        let first = sanitize_span_text(first_content_line(text));
                        let preview =
                            truncate_chars(&first, width.saturating_sub(marker.chars().count()));
                        out.push(Line::from(vec![
                            Span::styled(marker, style),
                            Span::styled(preview, style),
                        ]));
                    }
                }
                LogEntry::ToolCall {
                    id: _,
                    name,
                    summary,
                    result,
                } => {
                    let (bullet, bullet_style) = match result {
                        None => (
                            SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()],
                            Style::default().fg(Color::Yellow),
                        ),
                        Some(o) if o.is_error => ('⏺', Style::default().fg(Color::Red)),
                        Some(_) => ('⏺', Style::default().fg(Color::Green)),
                    };
                    // Sanitize before measuring: a multi-line Bash command or a
                    // tab inside the summary would otherwise garble the row.
                    let summary_flat = sanitize_span_text(summary);
                    let header_summary = if self.expanded {
                        summary_flat
                    } else {
                        // bullet+space (2) + name + parens (2)
                        truncate_chars(
                            &summary_flat,
                            width.saturating_sub(name.chars().count() + 4),
                        )
                    };
                    out.push(Line::from(vec![
                        Span::styled(format!("{bullet} "), bullet_style),
                        Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(format!("({header_summary})")),
                    ]));
                    match result {
                        None => out.push(Line::from(Span::styled(
                            "  ⎿ running…",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ))),
                        Some(outcome) => {
                            let elbow_style = if outcome.is_error {
                                Style::default().fg(Color::Red)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            };
                            if self.expanded || tools::always_expand(name) {
                                emit_expanded_result(&mut out, &outcome.content, elbow_style);
                            } else {
                                // Errors are always previewed — the message is
                                // the signal — even for count-only tools.
                                let preview = outcome.is_error || tools::preview_output(name);
                                emit_collapsed_result(
                                    &mut out,
                                    &outcome.content,
                                    elbow_style,
                                    width,
                                    preview,
                                );
                            }
                        }
                    }
                }
                LogEntry::Usage {
                    in_tokens,
                    out_tokens,
                    cache_write,
                    cache_read,
                } => {
                    out.push(Line::from(Span::styled(
                        format!(
                            "[usage] in={in_tokens} out={out_tokens} cache_write={cache_write} cache_read={cache_read}"
                        ),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                LogEntry::Allow(name) => {
                    out.push(Line::from(Span::styled(
                        format!("[allow] {name}"),
                        Style::default().fg(Color::Green),
                    )));
                }
                LogEntry::Deny(name) => {
                    out.push(Line::from(Span::styled(
                        format!("[deny] {name}"),
                        Style::default().fg(Color::Red),
                    )));
                }
                LogEntry::Info(text) => {
                    out.push(Line::from(Span::styled(
                        format!("[info] {}", sanitize_span_text(text)),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                LogEntry::Warn(text) => {
                    out.push(Line::from(Span::styled(
                        format!("[warn] {}", sanitize_span_text(text)),
                        Style::default().fg(Color::Yellow),
                    )));
                }
                LogEntry::Error(text) => {
                    out.push(Line::from(Span::styled(
                        format!("[error] {}", sanitize_span_text(text)),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
        }
        out
    }

    // Build the /background pair-panel body: the scannable QR, the paste-able code
    // wrapped to width, and an action hint. The QR is forced black-on-white so it
    // scans regardless of the terminal theme — the Dense1x2 half-blocks encode dark
    // modules as the glyph (`█`/`▀`/`▄`), which only reads as a valid QR when drawn
    // dark-on-light; a light-on-dark terminal would otherwise invert it.
    fn pair_panel_body(&self, inner_width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let qr_style = Style::default().fg(Color::Black).bg(Color::White);
        if let Some(qr) = &self.pairing_qr {
            for line in qr.lines() {
                out.push(Line::from(Span::styled(line.to_string(), qr_style)));
            }
        }
        out.push(Line::from(""));
        if let Some(code) = &self.pairing_code {
            // Pre-wrap the long code by hand so the Paragraph needs no `Wrap` — which
            // would also mangle the fixed-width QR rows above.
            let w = inner_width.max(1);
            let chars: Vec<char> = code.chars().collect();
            for chunk in chars.chunks(w) {
                let s: String = chunk.iter().collect();
                out.push(Line::from(Span::styled(
                    s,
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "Scan to control from your phone · Enter = foreground · Ctrl-C = quit",
            Style::default().fg(Color::Yellow),
        )));
        out
    }

    // The non-QR background panel: shown while detached when there's no scannable QR
    // to display — the relay is still dialing or failed, no relay is configured, or
    // this is a guest. Mirrors the four background states the layout sizes for.
    fn background_banner_body(&self) -> Vec<Line<'static>> {
        let hint = Line::from(Span::styled(
            "Enter = foreground · Ctrl-C = quit",
            Style::default().fg(Color::Yellow),
        ));
        if self.pairing_qr.is_some() {
            // Owner with a relay configured: report the dial's progress.
            let msg = match &self.handoff_status {
                Some(HandoffStatus::Failed(e)) => Line::from(Span::styled(
                    format!("relay unreachable — phone handoff unavailable ({e})"),
                    Style::default().fg(Color::Red),
                )),
                // Connecting, or no update yet. (Connected renders the QR panel, not this.)
                _ => Line::from(Span::styled(
                    "connecting to relay…",
                    Style::default().fg(Color::DarkGray),
                )),
            };
            vec![msg, hint]
        } else if self.is_owner {
            // Owner without a relay: backgrounding still pauses, but no phone handoff.
            vec![
                Line::from(Span::styled(
                    "backgrounded — phone handoff off (set NUDGE_RELAY to enable it)",
                    Style::default().fg(Color::DarkGray),
                )),
                hint,
            ]
        } else {
            // A --connect guest: detaching just pauses the local view.
            vec![Line::from(Span::styled(
                "⏸  backgrounded — Enter to foreground · Ctrl-C to quit",
                Style::default().fg(Color::DarkGray),
            ))]
        }
    }

    fn render(&mut self, f: &mut ratatui::Frame) {
        // Dynamic input height — grow up to MAX_INPUT_LINES display rows, then
        // scroll inside the input box. Rows are counted post-wrap (a long line
        // occupies several rows), so the box grows as text wraps, not only on
        // explicit newlines. Single-row input keeps the box at 3 rows so the
        // log gets maximum real estate.
        let input_inner_width = f.area().width.saturating_sub(2).max(1) as usize;
        let (input_rows, cursor_row, cursor_col) =
            wrap_input(&self.input, self.cursor, input_inner_width);
        let total_input_lines = input_rows.len();
        let visible_input_lines = total_input_lines.clamp(1, MAX_INPUT_LINES);
        let input_height = (visible_input_lines as u16) + 2;
        let input_scroll = total_input_lines.saturating_sub(MAX_INPUT_LINES) as u16;

        // Once the relay dial connects, /background swaps the input box for a pair
        // panel (QR + code + hint), sized to fit the QR. The conversation log stays
        // above it (Min(0) — it just shrinks), so context is kept rather than blown
        // away by a full-screen takeover. Before connect / on failure / with no relay,
        // a short banner takes its place instead (see background_banner_body).
        let show_qr = self.mode == Mode::Background
            && self.pairing_qr.is_some()
            && matches!(self.handoff_status, Some(HandoffStatus::Connected));
        let (log_constraint, bottom_height) = if show_qr {
            let qr_rows = self
                .pairing_qr
                .as_ref()
                .map(|q| q.lines().count())
                .unwrap_or(0);
            let code_rows = self
                .pairing_code
                .as_ref()
                .map(|c| c.chars().count().div_ceil(input_inner_width))
                .unwrap_or(0);
            // borders(2) + qr + blank(1) + code + blank(1) + hint(1). The QR sits at
            // the top of the panel, so if the terminal is too short the hint/code
            // clip first and the QR stays scannable.
            (Constraint::Min(0), (qr_rows + code_rows + 5) as u16)
        } else if self.mode == Mode::Background {
            // Short banner: a message line + the foreground/quit hint, inside borders.
            (Constraint::Min(3), 4)
        } else {
            (Constraint::Min(3), input_height)
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                log_constraint,
                Constraint::Length(1),
                Constraint::Length(bottom_height),
            ])
            .split(f.area());

        let log_area = chunks[0];
        let status_area = chunks[1];
        let input_area = chunks[2];

        let inner_width = log_area.width.saturating_sub(2);
        let visible = log_area.height.saturating_sub(2);
        let lines = self.render_log(inner_width as usize);

        // Count post-wrap rows, not source lines. With `Wrap { trim: false }`,
        // any line longer than `inner_width` renders as multiple rows, but
        // `lines.len()` would only count the source line. Using that count as
        // max_scroll leaves auto-scroll stranded above the true bottom — most
        // visible on bulk resume, where hundreds of wrapping entries land at
        // once and the last few turns end up below the viewport.
        let log_paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = log_paragraph.line_count(inner_width) as u16;
        let max_scroll = total.saturating_sub(visible);
        if self.auto_scroll {
            self.scroll = max_scroll;
        } else if self.scroll >= max_scroll {
            // User scrolled past the bottom — snap and re-engage tail-follow.
            self.scroll = max_scroll;
            self.auto_scroll = true;
        }

        // Identity info, highest priority first — ratatui clips the title at
        // the border width, so on narrow terminals the tail drops first.
        let git_tag = match &self.git_branch {
            Some(branch) => format!("git:{branch}"),
            None => "no git".into(),
        };
        // Leading role badge so it survives title clipping: `owner` = this terminal
        // hosts the agent and can force-reclaim from a guest; `guest` = a --connect
        // client (local socket or remote phone/TUI) that can drive but not reclaim.
        let (badge, badge_style) = if self.is_owner {
            (
                " owner ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (
                " guest ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        };
        // Prefer the human name; fall back to the uuid for a still-nameless session.
        let session_label = self.session_name.as_deref().unwrap_or(&self.session_id);
        let title = Line::from(vec![
            Span::styled(badge, badge_style),
            Span::raw(format!(
                " nudge · {} · {} · {git_tag} · {} · {}",
                session_label, self.cwd_display, self.model, self.platform
            )),
        ]);
        let log = log_paragraph
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((self.scroll, 0));
        f.render_widget(log, log_area);

        let mut status_text = if self.status.is_empty() {
            String::new()
        } else {
            let frame = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
            format!("{frame} {}…", self.status)
        };
        if !self.auto_scroll && max_scroll > 0 {
            if !status_text.is_empty() {
                status_text.push_str("  ");
            }
            status_text.push_str("(scrolled — End to follow)");
        }
        // Mode tags live right-aligned on the status row (they used to crowd
        // the title bar). Dropped entirely when the left side needs the room.
        let mut mode_tags = format!("thinking: {}", self.thinking_display);
        if self.expanded {
            mode_tags.push_str(" · expanded");
        }
        let status_width = status_area.width as usize;
        let left_len = status_text.chars().count();
        let right_len = mode_tags.chars().count();
        if left_len + right_len + 2 <= status_width {
            let pad = status_width - left_len - right_len;
            status_text.push_str(&" ".repeat(pad));
            status_text.push_str(&mode_tags);
        }
        let status = Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, status_area);

        if show_qr {
            // Relay armed: render the scannable QR + paste code in-screen above
            // where the input box was. No alt-screen exit — the conversation stays
            // visible above. No cursor (we're detached).
            let body = self.pair_panel_body(input_area.width.saturating_sub(2) as usize);
            let panel = Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("pair a device"),
            );
            f.render_widget(panel, input_area);
        } else if self.mode == Mode::Background {
            // No QR to show yet (dialing / failed / no relay / guest): a short banner
            // replaces the input box; the log above stays frozen as at detach.
            let banner = Paragraph::new(self.background_banner_body())
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("paused"));
            f.render_widget(banner, input_area);
        } else {
            // Pre-wrapped into display rows (see wrap_input), so the Paragraph
            // needs no `Wrap` — and the cursor mapping below stays exact, which
            // ratatui's word-wrap would not allow.
            let input_body: Vec<Line> = input_rows.into_iter().map(Line::from).collect();
            let input = Paragraph::new(input_body)
                .scroll((input_scroll, 0))
                .block(
                    Block::default().borders(Borders::ALL).title(
                        "message (Enter=send · Alt+Enter or \\<Enter>=newline · Ctrl-O=expand · /background · Ctrl-C=quit)",
                    ),
                );
            f.render_widget(input, input_area);

            // cursor_row / cursor_col are display coordinates from wrap_input;
            // subtract the in-box scroll to land on the visible row.
            let cursor_row_vis = (cursor_row as u16).saturating_sub(input_scroll);
            f.set_cursor_position(Position {
                x: input_area.x + 1 + cursor_col as u16,
                y: input_area.y + 1 + cursor_row_vis,
            });
        }

        if let Some(sel) = self.model_picker {
            let popup = centered_rect(50, 30, f.area());
            f.render_widget(Clear, popup);
            let mut body = Vec::new();
            for (i, (label, id)) in crate::MODELS.iter().enumerate() {
                let marker = if i == sel { "❯ " } else { "  " };
                let current = if *id == self.model { "  (current)" } else { "" };
                let style = if i == sel {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                body.push(Line::from(Span::styled(
                    format!("{marker}{label}  — {id}{current}"),
                    style,
                )));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "↑/↓ move · Enter select · Esc cancel",
                Style::default().fg(Color::DarkGray),
            )));
            let para = Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title("select model"));
            f.render_widget(para, popup);
        }

        // Drawn after the model picker so a permission prompt arriving while
        // the picker is open sits on top — matching key handling, where the
        // pending permission also takes priority.
        if let Some(p) = &self.pending {
            let popup = centered_rect(60, 30, f.area());
            f.render_widget(Clear, popup);
            let mut body = vec![
                Line::from(vec![
                    Span::styled("Tool: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(p.tool_name.clone()),
                ]),
                Line::from(""),
            ];
            // Multi-line summaries (e.g. a multi-line Bash command) become one
            // Line per row — a raw `\n` inside a Line garbles the popup.
            for line in p.summary.lines() {
                body.push(Line::from(sanitize_span_text(line)));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "Allow?  [y]es  /  [N]o",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            let para = Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title("permission"))
                .wrap(Wrap { trim: false });
            f.render_widget(para, popup);
        }
    }
}

// Renders multi-line text with a marker on the first line and a 2-space
// continuation indent on subsequent lines, so visual grouping survives wrap.
fn emit_prefixed(out: &mut Vec<Line<'static>>, text: &str, marker: &str, marker_style: Style) {
    let mut first = true;
    for line in text.lines() {
        out.push(Line::from(vec![
            Span::styled(
                if first {
                    marker.to_string()
                } else {
                    "  ".to_string()
                },
                marker_style,
            ),
            Span::raw(sanitize_span_text(line)),
        ]));
        first = false;
    }
    if first {
        // Empty text — still emit the marker so the turn is visible.
        out.push(Line::from(Span::styled(marker.to_string(), marker_style)));
    }
}

// Like emit_prefixed, but for already-styled lines (markdown-rendered
// assistant text): prepend the turn marker to the first line and a 2-space
// continuation indent to the rest, preserving each line's existing spans.
fn emit_prefixed_lines(
    out: &mut Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    marker: &str,
    marker_style: Style,
) {
    if body.is_empty() {
        out.push(Line::from(Span::styled(marker.to_string(), marker_style)));
        return;
    }
    for (i, line) in body.into_iter().enumerate() {
        let lead = if i == 0 {
            marker.to_string()
        } else {
            "  ".to_string()
        };
        let mut spans = line.spans;
        spans.insert(0, Span::styled(lead, marker_style));
        out.push(Line::from(spans));
    }
}

// Ratatui spans must contain only printable text: it copies each char into a
// terminal cell verbatim, so a literal `\t` / `\n` / other control char gets
// re-interpreted by the terminal during repaints, shifting every later cell —
// the on-screen garbage then *changes* as scrolling repaints different
// regions. Every piece of foreign text (tool summaries, tool output, model
// text) must pass through here before entering a Span.
fn sanitize_span_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\n' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// First non-empty line of a block, trimmed — the most informative single-line
// preview for collapsed views (leading blank lines would render as nothing).
fn first_content_line(content: &str) -> &str {
    content
        .lines()
        .map(str::trim_end)
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

fn emit_expanded_thinking(out: &mut Vec<Line<'static>>, content: &str, style: Style) {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        out.push(Line::from(Span::styled("✻ Thinking… (empty)", style)));
        return;
    }
    out.push(Line::from(Span::styled("✻ Thinking…", style)));
    let shown = total.min(EXPANDED_MAX_LINES);
    for line in lines.iter().take(shown) {
        out.push(Line::from(Span::styled(
            format!("  {}", sanitize_span_text(line)),
            style,
        )));
    }
    if total > shown {
        out.push(Line::from(Span::styled(
            format!("  +{} more lines", total - shown),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
}

// Collapsed tool result: a single `  ⎿ <preview>` row, hard-truncated to the
// viewport width so it can never wrap, with a hint when there is more to see.
// With `preview: false`, multi-line output collapses to a bare line count —
// for tools whose output is positional noise out of context (Read/Grep/Glob).
// Single-line outputs (e.g. Grep's "(no matches)") are shown verbatim either
// way: a count of "1 line" would hide the more informative message.
fn emit_collapsed_result(
    out: &mut Vec<Line<'static>>,
    content: &str,
    elbow_style: Style,
    width: usize,
    preview: bool,
) {
    const ELBOW: &str = "  ⎿ ";
    let first = sanitize_span_text(first_content_line(content));
    if first.is_empty() {
        out.push(Line::from(vec![
            Span::styled(ELBOW, elbow_style),
            Span::styled("(no output)", Style::default().fg(Color::DarkGray)),
        ]));
        return;
    }
    let total = content.lines().count();
    if !preview && total > 1 {
        out.push(Line::from(vec![
            Span::styled(ELBOW, elbow_style),
            Span::styled(
                format!("{total} lines (ctrl+o to expand)"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
        return;
    }
    let extra = total.saturating_sub(1);
    let mut suffix = if extra > 0 {
        format!("  (+{extra} lines · ctrl+o to expand)")
    } else {
        String::new()
    };
    let mut budget = width.saturating_sub(ELBOW.chars().count() + suffix.chars().count());
    if suffix.is_empty() && first.chars().count() > budget {
        // Single-line output that still doesn't fit — the hint replaces the
        // line count as the "there's more" signal.
        suffix = "  (ctrl+o to expand)".into();
        budget = width.saturating_sub(ELBOW.chars().count() + suffix.chars().count());
    }
    out.push(Line::from(vec![
        Span::styled(ELBOW, elbow_style),
        Span::raw(truncate_chars(&first, budget)),
        Span::styled(
            suffix,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ),
    ]));
}

fn emit_expanded_result(out: &mut Vec<Line<'static>>, content: &str, elbow_style: Style) {
    const ELBOW: &str = "  ⎿ ";
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        out.push(Line::from(vec![
            Span::styled(ELBOW, elbow_style),
            Span::styled("(no output)", Style::default().fg(Color::DarkGray)),
        ]));
        return;
    }
    let shown = total.min(EXPANDED_MAX_LINES);
    for (i, line) in lines.iter().take(shown).enumerate() {
        if i == 0 {
            out.push(Line::from(vec![
                Span::styled(ELBOW, elbow_style),
                Span::raw(sanitize_span_text(line)),
            ]));
        } else {
            out.push(Line::from(format!("    {}", sanitize_span_text(line))));
        }
    }
    if total > shown {
        out.push(Line::from(Span::styled(
            format!("    +{} more lines", total - shown),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
}

// Wrap the input buffer into display rows at `width` chars and locate the edit
// cursor within them. Character-based wrap (not word wrap) so a row maps to an
// exact cell range and the rendered cursor lands precisely. Each logical line
// (split on '\n') yields at least one row, so blank lines still take space.
// `cursor` is a char index into `input`; the returned (row, col) are display
// coordinates. When the buffer ends on a row filled exactly to `width`, a
// trailing empty row is added so the cursor has a cell to sit in.
fn wrap_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    let mut cursor_located = false;
    let mut remaining = cursor; // chars still to consume to reach the cursor

    for logical in input.split('\n') {
        let chars: Vec<char> = logical.chars().collect();
        let len = chars.len();
        let line_start_row = rows.len();
        if len == 0 {
            rows.push(String::new());
        } else {
            let mut start = 0;
            while start < len {
                let end = (start + width).min(len);
                rows.push(chars[start..end].iter().collect());
                start = end;
            }
        }
        if !cursor_located && remaining <= len {
            cursor_row = line_start_row + remaining / width;
            cursor_col = remaining % width;
            cursor_located = true;
        } else if !cursor_located {
            remaining -= len + 1; // line chars plus the '\n' separator
        }
    }

    // Cursor sits one row past the content (end of a width-filled final line, or
    // an unexpected out-of-range index): give it a row to render in.
    while cursor_row >= rows.len() {
        rows.push(String::new());
    }
    (rows, cursor_row, cursor_col)
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub async fn run<H: SessionHandle>(
    host: &H,
    cfg: UiConfig,
    initial: Controller,
    handoff_rx: Option<mpsc::Receiver<HandoffStatus>>,
) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, host, cfg, initial, handoff_rx).await;
    restore_terminal(&mut terminal)?;
    result
}

// Receive from the controller stream, or never resolve while detached — lets the
// `select!` event branch be a single expression that holds `events` only for its
// own future, while the transition handling (after the select) reassigns it.
async fn recv_events(
    events: &mut Option<mpsc::Receiver<ControllerEvent>>,
) -> Option<ControllerEvent> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

// Same shape as `recv_events` for the relay-dial status stream: never resolves when
// there's no relay configured (the `None` arm), so the `select!` branch is inert.
async fn recv_handoff(rx: &mut Option<mpsc::Receiver<HandoffStatus>>) -> Option<HandoffStatus> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn run_loop<H: SessionHandle>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    host: &H,
    cfg: UiConfig,
    initial: Controller,
    mut handoff_rx: Option<mpsc::Receiver<HandoffStatus>>,
) -> Result<()> {
    let mut app = App::new(cfg);
    // No front-end-side transcript replay: the host seeds its buffer with the
    // resumed history, so the initial attach below streams history + live as one
    // event stream that the renderer treats uniformly.

    // Some while attached (Foreground), None while detached (Background). The
    // pair is swapped at background/foreground transitions below.
    let mut events: Option<mpsc::Receiver<ControllerEvent>> = Some(initial.events);
    let mut ui_tx: Option<mpsc::Sender<UiEvent>> = Some(initial.ui_tx);

    let mut input_stream = EventStream::new();
    let mut tick = tokio::time::interval(SPINNER_TICK);
    // If we fall behind (long-running render, big paste handling), don't
    // burn CPU catching up on missed ticks — just skip to the next slot.
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    terminal.draw(|f| app.render(f))?;

    loop {
        let mut redraw = true;
        tokio::select! {
            maybe_event = recv_events(&mut events) => {
                match maybe_event {
                    Some(event) => {
                        app.handle_agent_event(event);
                        // Coalesce: a bulk replay (resume / foreground attach)
                        // floods the channel; draining everything already queued
                        // before the single redraw below turns N full re-renders
                        // into ~1. Live events arrive sparsely, so try_recv is
                        // empty immediately and per-event rendering is unchanged.
                        if let Some(rx) = events.as_mut() {
                            while let Ok(event) = rx.try_recv() {
                                app.handle_agent_event(event);
                            }
                        }
                    }
                    None => break, // foreground stream closed (loop/session ended)
                }
            }
            maybe_status = recv_handoff(&mut handoff_rx) => {
                match maybe_status {
                    // The dial task reports its progress; the background screen shows it.
                    Some(status) => app.handoff_status = Some(status),
                    // All senders dropped (shouldn't happen while the host lives): stop
                    // polling so the branch doesn't spin on a closed channel.
                    None => {
                        handoff_rx = None;
                        redraw = false;
                    }
                }
            }
            maybe_input = input_stream.next() => {
                match maybe_input {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.mode {
                            Mode::Foreground => {
                                if let Some(tx) = ui_tx.as_ref() {
                                    app.handle_key(key, tx);
                                }
                            }
                            Mode::Background => app.handle_background_key(key),
                        }
                    }
                    Some(Ok(Event::Paste(text))) if app.mode == Mode::Foreground => {
                        app.handle_paste(text);
                    }
                    Some(Ok(Event::Mouse(ev))) if app.mode == Mode::Foreground => {
                        if !app.handle_mouse(ev) {
                            // Mouse motion / button events we don't act on —
                            // skip the redraw to keep the terminal quiet.
                            redraw = false;
                        }
                    }
                    Some(Ok(_)) => { redraw = false; }
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                }
            }
            _ = tick.tick() => {
                if app.status.is_empty() {
                    // Idle — no spinner to animate, skip the redraw to keep
                    // the terminal quiet.
                    redraw = false;
                } else {
                    app.spinner_frame = app.spinner_frame.wrapping_add(1);
                }
            }
        }

        // Apply a requested background/foreground switch (here, where the
        // SessionHost handle and the channel slots live).
        if let Some(target) = app.pending_transition.take() {
            match target {
                Mode::Background => {
                    // Detaching fires the host's handoff hook on the first
                    // /background — binding the local socket, or (relay armed)
                    // spawning the relay dial-out so a phone can attach. The QR (if
                    // armed) is rendered in-screen above the banner — no alt-screen
                    // exit, so the conversation stays visible.
                    host.detach();
                    events = None;
                    ui_tx = None;
                    app.enter_background();
                }
                Mode::Foreground => {
                    // Force-takeover (no-op for a guest — `attach_force` defaults to
                    // a plain attach off the in-process host): the local owner TUI is
                    // the only front-end allowed to reclaim a session a remote
                    // controller (e.g. a pocketed phone) is holding.
                    match host.attach_force().await {
                        Some(c) => {
                            // Rebuild from the event stream: enter_foreground clears
                            // the log, then the broker replays its full buffer
                            // (seeded history + everything since) as events.
                            app.enter_foreground();
                            events = Some(c.events);
                            ui_tx = Some(c.ui_tx);
                        }
                        None => {
                            // Owner force only fails if the broker is gone; a guest's
                            // non-forcing attach fails because another controller holds it.
                            let why = if app.is_owner {
                                "could not foreground — session has ended"
                            } else {
                                "could not foreground — another controller holds the session"
                            };
                            app.push(LogEntry::Warn(why.into()));
                        }
                    }
                }
            }
        }

        if app.quit {
            break;
        }
        if redraw {
            terminal.draw(|f| app.render(f))?;
        }
    }

    Ok(())
}

// Enter the TUI's alternate screen + input modes. Factored out so the pair-screen
// resume can re-enter it without duplicating the escape sequences. Raw mode is
// owned separately (it stays on across a pair-screen suspend so the EventStream
// keeps delivering keys), so it's not toggled here.
fn enter_screen<W: Write>(w: &mut W) -> io::Result<()> {
    execute!(
        w,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // Opt into the kitty keyboard protocol so terminals that support it
    // (Kitty, WezTerm, Ghostty, foot, Alacritty ≥ 0.13, iTerm2 ≥ 3.5, …)
    // report Shift+Enter / Ctrl+Enter as distinct events instead of
    // collapsing them to bare CR. Best-effort: terminals that don't
    // understand the CSI sequence silently ignore it, so failures here
    // are not fatal and we don't surface an error.
    let _ = execute!(
        w,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        ),
    );
    Ok(())
}

// Leave the alternate screen + input modes (the inverse of `enter_screen`). Used
// both on final teardown and to drop to the plain-terminal pair screen.
fn leave_screen<W: Write>(w: &mut W) -> io::Result<()> {
    let _ = execute!(w, PopKeyboardEnhancementFlags);
    execute!(
        w,
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    enter_screen(&mut stdout)?;

    // Restore terminal on panic so a bug doesn't leave the user's shell broken.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = leave_screen(&mut io::stdout());
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    leave_screen(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}
