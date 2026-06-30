use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use crate::core::UiEvent;

use super::app::{App, LogEntry, Mode};

impl App {
    pub(super) fn handle_background_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.pending_transition = Some(Mode::Foreground),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            _ => {}
        }
    }

    // Byte offset for the char cursor; clamps to the end so it can't panic.
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

    // Char index of the cursor's line start (powers Ctrl-A).
    fn line_start(&self) -> usize {
        let mut start = 0;
        for (i, c) in self.input.chars().take(self.cursor).enumerate() {
            if c == '\n' {
                start = i + 1;
            }
        }
        start
    }

    // Char index of the cursor's line end (powers Ctrl-E).
    fn line_end(&self) -> usize {
        self.input
            .chars()
            .enumerate()
            .skip(self.cursor)
            .find(|(_, c)| *c == '\n')
            .map(|(i, _)| i)
            .unwrap_or_else(|| self.input.chars().count())
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent, ui_tx: &mpsc::Sender<UiEvent>) {
        // DISAMBIGUATE_ESCAPE_CODES also reports Release/Repeat — ignore non-Press.
        if key.kind != KeyEventKind::Press {
            return;
        }
        if let Some(mut pending) = self.pending.take() {
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
                KeyCode::Up => {
                    pending.scroll = pending.scroll.saturating_sub(1);
                    self.pending = Some(pending);
                }
                KeyCode::Down => {
                    pending.scroll = pending.scroll.saturating_add(1);
                    self.pending = Some(pending);
                }
                KeyCode::PageUp => {
                    pending.scroll = pending.scroll.saturating_sub(10);
                    self.pending = Some(pending);
                }
                KeyCode::PageDown => {
                    pending.scroll = pending.scroll.saturating_add(10);
                    self.pending = Some(pending);
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
                self.expanded = !self.expanded;
            }
            // Alt/Ctrl+Enter = newline. Shift+Enter is unsupported: the kitty flag that
            // would disambiguate it breaks IME composition in crossterm 0.28.
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                self.insert_char('\n');
            }
            // Ctrl+J fallback: terminals without the kitty protocol send Ctrl+Enter as
            // bare CR (indistinguishable from Enter), but Ctrl+J (LF) is unambiguous.
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_char('\n');
            }
            // Trailing-backslash continuation (POSIX parity: odd count = newline, even = submit).
            // The always-available newline escape hatch, independent of terminal quirks.
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
                // Single-line `/...` is a slash command; multi-line starting with `/` is a message.
                if trimmed.starts_with('/') && !trimmed.contains('\n') {
                    let cmd = trimmed.to_string();
                    self.clear_input();
                    self.handle_command(&cmd, ui_tx);
                    return;
                }
                let text = std::mem::take(&mut self.input);
                self.cursor = 0;
                // Rendered on the broker's echo, not here, so live and replay match.
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
            }
            KeyCode::Home => {
                self.auto_scroll = false;
                self.scroll = 0;
            }
            KeyCode::End => {
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
            // The registry lives in the agent task, so results come back as a Notice.
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
            // Bare = daemon derives the name; the final label returns via Notice + SessionInfo.
            Some("/session-rename") => {
                let arg = cmd["/session-rename".len()..].trim();
                let name = (!arg.is_empty()).then(|| arg.to_string());
                let _ = ui_tx.try_send(UiEvent::RenameSession { name });
            }
            // The run loop performs the detach (it holds the SessionHost).
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

    // Returns true when visible state changed, so the caller can skip motion-only redraws.
    pub(super) fn handle_mouse(&mut self, ev: MouseEvent) -> bool {
        // A permission popup is modal, so the wheel scrolls it rather than the log behind.
        if let Some(pending) = &mut self.pending {
            match ev.kind {
                MouseEventKind::ScrollUp => {
                    pending.scroll = pending.scroll.saturating_sub(3);
                    return true;
                }
                MouseEventKind::ScrollDown => {
                    pending.scroll = pending.scroll.saturating_add(3);
                    return true;
                }
                _ => return false,
            }
        }
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

    // Bracketed paste arrives as one event (newlines intact). Normalize tabs/control
    // chars here, since a literal tab garbles cells and desyncs the cursor math.
    pub(super) fn handle_paste(&mut self, text: String) {
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
}

// Character-wrap the input into display rows and locate the cursor within them.
// Char-based (not word) so a row maps to an exact cell range and the cursor lands
// precisely. Each logical line yields >=1 row; a width-filled final line gets a
// trailing empty row so the cursor has a cell to sit in.
pub(super) fn wrap_input(input: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    let mut cursor_located = false;
    let mut remaining = cursor;

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

    while cursor_row >= rows.len() {
        rows.push(String::new());
    }
    (rows, cursor_row, cursor_col)
}
