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
            // Toggle between the full-access and watch-only pairing QR (both legs are
            // already dialed); a no-op when there's no watch pairing to show.
            KeyCode::Char('w') if self.has_watch_pairing() => {
                self.show_watch = !self.show_watch;
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

    // Byte offset for an arbitrary char index; clamps to the end so it can't panic.
    fn byte_at(&self, char_idx: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len())
    }

    // Remove chars [start, end) and leave the cursor at start.
    fn delete_chars(&mut self, start: usize, end: usize) {
        let range = self.byte_at(start)..self.byte_at(end);
        self.input.replace_range(range, "");
        self.cursor = start;
    }

    // Char index of the previous word's start: skip trailing whitespace, then the
    // word itself (readline Ctrl-W semantics).
    fn prev_word_start(&self) -> usize {
        let chars: Vec<char> = self.input.chars().take(self.cursor).collect();
        let mut i = chars.len();
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    // Move the cursor one wrapped display row up/down, keeping the column where
    // possible (clamped to the target row's length).
    fn move_cursor_row(&mut self, delta: isize) {
        let width = self.input_view_width.max(1);
        let (rows, row, col) = wrap_input(&self.input, self.cursor, width);
        let target = row as isize + delta;
        if target < 0 || target as usize >= rows.len() {
            return;
        }
        self.cursor = char_index_at(&self.input, target as usize, col, width);
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
                    self.model_picker = Some((sel + 1).min(self.models.len() - 1));
                }
                KeyCode::Enter => {
                    self.model_picker = None;
                    let (label, id) = self.models[sel].clone();
                    if id != self.model {
                        self.model = id.clone();
                        self.push(LogEntry::Info(format!("model set to {label} ({id})")));
                        self.push(LogEntry::Blank);
                        self.auto_scroll = true;
                        let _ = ui_tx.try_send(UiEvent::Command {
                            line: format!("/model {id}"),
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
                if trimmed.eq_ignore_ascii_case("exit") {
                    let _ = ui_tx.try_send(UiEvent::Quit);
                    self.quit = true;
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
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_chars(self.line_start(), self.cursor);
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_chars(self.cursor, self.line_end());
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_chars(self.prev_word_start(), self.cursor);
            }
            KeyCode::Up => self.move_cursor_row(-1),
            KeyCode::Down => self.move_cursor_row(1),
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

    // Client-local commands only. A bare `/model` opens the picker (rendered from the
    // daemon's Capabilities) and `/background` detaches — both are front-end UI. Every
    // other `/…` line — including `/model <id>` — is a session-level command parsed
    // server-side, so it's sent verbatim as UiEvent::Command and works identically on
    // any front-end.
    fn handle_command(&mut self, cmd: &str, ui_tx: &mpsc::Sender<UiEvent>) {
        let mut parts = cmd.split_whitespace();
        match parts.next() {
            Some("/model") if parts.next().is_none() => {
                if self.models.is_empty() {
                    self.push(LogEntry::Warn("model list not available yet".into()));
                    self.push(LogEntry::Blank);
                    self.auto_scroll = true;
                    return;
                }
                let current = self
                    .models
                    .iter()
                    .position(|(_, id)| *id == self.model)
                    .unwrap_or(0);
                self.model_picker = Some(current);
            }
            // The run loop performs the detach (it holds the SessionHost).
            Some("/background") | Some("/bg") => {
                self.pending_transition = Some(Mode::Background);
            }
            _ => {
                let _ = ui_tx.try_send(UiEvent::Command {
                    line: cmd.to_string(),
                });
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

// Inverse of wrap_input's cursor mapping: char index for a (display row, col),
// using the same char-wrap. col is clamped to the row's length.
fn char_index_at(input: &str, row: usize, col: usize, width: usize) -> usize {
    let width = width.max(1);
    let mut char_pos = 0;
    let mut first_row = 0;
    for logical in input.split('\n') {
        let len = logical.chars().count();
        let line_rows = if len == 0 { 1 } else { len.div_ceil(width) };
        if row < first_row + line_rows {
            let row_start = (row - first_row) * width;
            let row_len = (len - row_start).min(width);
            return char_pos + row_start + col.min(row_len);
        }
        first_row += line_rows;
        char_pos += len + 1; // line chars plus the '\n' separator
    }
    input.chars().count()
}

#[cfg(test)]
mod tests {
    use super::super::app::UiConfig;
    use super::*;

    fn test_app() -> App {
        App::new(UiConfig {
            session_id: "s".into(),
            session_name: None,
            model: "m".into(),
            thinking_display: "".into(),
            pairing_qr: None,
            pairing_code: None,
            pairing_qr_watch: None,
            pairing_code_watch: None,
            is_owner: true,
            user_name: "u".into(),
            models: Vec::new(),
        })
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    // `w` on the background screen toggles between the full-access and watch-only QR,
    // and each toggle flips which pairing `visible_pairing_*` returns.
    #[tokio::test]
    async fn background_w_toggles_the_visible_pairing() {
        let mut app = test_app();
        app.pairing_qr = Some("FULL_QR".into());
        app.pairing_code = Some("full-code".into());
        app.pairing_qr_watch = Some("WATCH_QR".into());
        app.pairing_code_watch = Some("watch-code".into());
        app.mode = Mode::Background;

        assert_eq!(
            app.visible_pairing_qr().map(String::as_str),
            Some("FULL_QR")
        );
        app.handle_background_key(press(KeyCode::Char('w')));
        assert!(app.show_watch);
        assert_eq!(
            app.visible_pairing_qr().map(String::as_str),
            Some("WATCH_QR")
        );
        assert_eq!(
            app.visible_pairing_code().map(String::as_str),
            Some("watch-code")
        );
        app.handle_background_key(press(KeyCode::Char('w')));
        assert!(!app.show_watch);
        assert_eq!(
            app.visible_pairing_qr().map(String::as_str),
            Some("FULL_QR")
        );
    }

    // With no watch-only pairing (a guest, or no relay), `w` does nothing.
    #[tokio::test]
    async fn background_w_is_a_noop_without_a_watch_pairing() {
        let mut app = test_app();
        app.pairing_qr = Some("FULL_QR".into());
        app.mode = Mode::Background;
        app.handle_background_key(press(KeyCode::Char('w')));
        assert!(!app.show_watch);
        assert_eq!(
            app.visible_pairing_qr().map(String::as_str),
            Some("FULL_QR")
        );
    }

    #[tokio::test]
    async fn typing_exit_quits_case_insensitively() {
        for word in ["exit", "ExIt"] {
            let mut app = test_app();
            let (tx, mut rx) = mpsc::channel(4);
            for c in word.chars() {
                app.handle_key(press(KeyCode::Char(c)), &tx);
            }
            app.handle_key(press(KeyCode::Enter), &tx);

            assert!(app.quit);
            assert!(matches!(rx.try_recv(), Ok(UiEvent::Quit)));
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn app_with_input(text: &str, cursor: usize, width: usize) -> App {
        let mut app = test_app();
        app.input = text.into();
        app.cursor = cursor;
        app.input_view_width = width;
        app
    }

    #[tokio::test]
    async fn up_down_move_across_logical_lines() {
        let mut app = app_with_input("abc\ndef", 5, 80);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(press(KeyCode::Up), &tx);
        assert_eq!(app.cursor, 1);
        app.handle_key(press(KeyCode::Down), &tx);
        assert_eq!(app.cursor, 5);
    }

    #[tokio::test]
    async fn up_down_move_across_wrapped_rows() {
        // width 4 wraps "abcdefghij" into "abcd" / "efgh" / "ij"
        let mut app = app_with_input("abcdefghij", 9, 4);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(press(KeyCode::Up), &tx);
        assert_eq!(app.cursor, 5);
        app.handle_key(press(KeyCode::Up), &tx);
        assert_eq!(app.cursor, 1);
        app.handle_key(press(KeyCode::Down), &tx);
        assert_eq!(app.cursor, 5);
    }

    #[tokio::test]
    async fn down_clamps_to_short_row() {
        let mut app = app_with_input("abcdefghij", 7, 4);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(press(KeyCode::Down), &tx);
        assert_eq!(app.cursor, 10);
    }

    #[tokio::test]
    async fn up_at_top_and_down_at_bottom_are_noops() {
        let mut app = app_with_input("abc\ndef", 1, 80);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(press(KeyCode::Up), &tx);
        assert_eq!(app.cursor, 1);
        app.cursor = 5;
        app.handle_key(press(KeyCode::Down), &tx);
        assert_eq!(app.cursor, 5);
    }

    #[tokio::test]
    async fn ctrl_u_deletes_to_line_start() {
        let mut app = app_with_input("ab\ncdef", 5, 80);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(ctrl('u'), &tx);
        assert_eq!(app.input, "ab\nef");
        assert_eq!(app.cursor, 3);
    }

    #[tokio::test]
    async fn ctrl_k_deletes_to_line_end() {
        let mut app = app_with_input("hello world\nnext", 5, 80);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(ctrl('k'), &tx);
        assert_eq!(app.input, "hello\nnext");
        assert_eq!(app.cursor, 5);
    }

    #[tokio::test]
    async fn ctrl_w_deletes_previous_word() {
        let mut app = app_with_input("hello world ", 12, 80);
        let (tx, _rx) = mpsc::channel(4);
        app.handle_key(ctrl('w'), &tx);
        assert_eq!(app.input, "hello ");
        assert_eq!(app.cursor, 6);
    }
}
