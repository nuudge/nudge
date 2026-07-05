use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::coding::tools;
use crate::core::HandoffStatus;

use super::app::{App, LogEntry, Mode};
use super::input::wrap_input;
use super::markdown;
use super::text::{
    centered_rect, emit_collapsed_result, emit_expanded_result, emit_expanded_thinking,
    emit_prefixed, emit_prefixed_lines, first_content_line, sanitize_span_text, truncate_chars,
};
use super::{MAX_INPUT_LINES, SPINNER_FRAMES};

impl App {
    // `width` is the log viewport's inner width — collapsed lines truncate to it.
    fn render_log(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for entry in &self.log {
            match entry {
                LogEntry::Blank => out.push(Line::from("")),
                LogEntry::User { text, sender } => {
                    // Own turns render as "> "; another party's turns are prefixed
                    // with their name so a shared session stays legible.
                    let prefix = if *sender == self.self_name {
                        "> ".to_string()
                    } else {
                        format!("{sender} > ")
                    };
                    emit_prefixed(
                        &mut out,
                        text,
                        &prefix,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    );
                }
                LogEntry::Assistant(text) => {
                    let body = markdown::render(text);
                    emit_prefixed_lines(&mut out, body, "* ", Style::default().fg(Color::Cyan))
                }
                LogEntry::Thinking(text) => {
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
                    // Sanitize before measuring so a tab/newline in the summary can't garble the row.
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
                                // Errors always preview, even for count-only tools.
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

    // QR forced black-on-white: the Dense1x2 half-blocks only scan when drawn
    // dark-on-light, so a light-on-dark terminal would otherwise invert it.
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
            // Hand-wrap so the Paragraph needs no `Wrap` (which would mangle the QR rows).
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

    // Background panel when there's no scannable QR (dialing/failed/no relay/guest).
    fn background_banner_body(&self) -> Vec<Line<'static>> {
        let hint = Line::from(Span::styled(
            "Enter = foreground · Ctrl-C = quit",
            Style::default().fg(Color::Yellow),
        ));
        if self.pairing_qr.is_some() {
            let msg = match &self.handoff_status {
                Some(HandoffStatus::Failed(e)) => Line::from(Span::styled(
                    format!("relay unreachable — phone handoff unavailable ({e})"),
                    Style::default().fg(Color::Red),
                )),
                _ => Line::from(Span::styled(
                    "connecting to relay…",
                    Style::default().fg(Color::DarkGray),
                )),
            };
            vec![msg, hint]
        } else if self.is_owner {
            vec![
                Line::from(Span::styled(
                    "backgrounded — phone handoff off (set NUDGE_RELAY to enable it)",
                    Style::default().fg(Color::DarkGray),
                )),
                hint,
            ]
        } else {
            vec![Line::from(Span::styled(
                "⏸  backgrounded — Enter to foreground · Ctrl-C to quit",
                Style::default().fg(Color::DarkGray),
            ))]
        }
    }

    pub(super) fn render(&mut self, f: &mut ratatui::Frame) {
        // Grow the input box up to MAX_INPUT_LINES post-wrap rows, then scroll inside it.
        let input_inner_width = f.area().width.saturating_sub(2).max(1) as usize;
        let (input_rows, cursor_row, cursor_col) =
            wrap_input(&self.input, self.cursor, input_inner_width);
        let total_input_lines = input_rows.len();
        let visible_input_lines = total_input_lines.clamp(1, MAX_INPUT_LINES);
        let input_height = (visible_input_lines as u16) + 2;
        let input_scroll = total_input_lines.saturating_sub(MAX_INPUT_LINES) as u16;

        // Once the relay connects, /background swaps the input box for the QR panel; the
        // log shrinks (Min(0)) rather than being taken over. Else a short banner.
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
            // borders(2) + qr + blank(1) + code + blank(1) + hint(1); QR on top so it
            // survives clipping on a short terminal.
            (Constraint::Min(0), (qr_rows + code_rows + 5) as u16)
        } else if self.mode == Mode::Background {
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

        // Count post-wrap rows, not source lines: with Wrap a long line spans several
        // rows, and using lines.len() would strand auto-scroll above the true bottom.
        let log_paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = log_paragraph.line_count(inner_width) as u16;
        let max_scroll = total.saturating_sub(visible);
        if self.auto_scroll {
            self.scroll = max_scroll;
        } else if self.scroll >= max_scroll {
            self.scroll = max_scroll;
            self.auto_scroll = true;
        }

        let git_tag = match &self.git_branch {
            Some(branch) => format!("git:{branch}"),
            None => "no git".into(),
        };
        // Leading badge so the role survives title clipping on narrow terminals.
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
        // Mode tags right-aligned on the status row; dropped when the left side needs the room.
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
            let body = self.pair_panel_body(input_area.width.saturating_sub(2) as usize);
            let panel = Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("pair a device"),
            );
            f.render_widget(panel, input_area);
        } else if self.mode == Mode::Background {
            let banner = Paragraph::new(self.background_banner_body())
                .wrap(Wrap { trim: true })
                .block(Block::default().borders(Borders::ALL).title("paused"));
            f.render_widget(banner, input_area);
        } else {
            // Pre-wrapped into rows (see wrap_input) so the Paragraph needs no `Wrap`,
            // keeping the cursor mapping below exact.
            let input_body: Vec<Line> = input_rows.into_iter().map(Line::from).collect();
            let input = Paragraph::new(input_body)
                .scroll((input_scroll, 0))
                .block(
                    Block::default().borders(Borders::ALL).title(
                        "message (Enter=send · Alt+Enter or \\<Enter>=newline · Ctrl-O=expand · /background · Ctrl-C=quit)",
                    ),
                );
            f.render_widget(input, input_area);

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

        // Drawn last so a permission prompt sits on top of an open model picker.
        if let Some(p) = &mut self.pending {
            let popup = centered_rect(60, 30, f.area());
            f.render_widget(Clear, popup);
            let mut body = vec![
                Line::from(vec![
                    Span::styled("Tool: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(p.tool_name.clone()),
                ]),
                Line::from(""),
            ];
            for line in p.summary.lines() {
                body.push(Line::from(sanitize_span_text(line)));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "Allow?  [y]es  /  [N]o",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            let para = Paragraph::new(body).wrap(Wrap { trim: false });
            let inner_width = popup.width.saturating_sub(2);
            let visible = popup.height.saturating_sub(2);
            let max_scroll = (para.line_count(inner_width) as u16).saturating_sub(visible);
            p.scroll = p.scroll.min(max_scroll);
            let title = if max_scroll > 0 {
                "permission (↑/↓ scroll)"
            } else {
                "permission"
            };
            let para = para
                .block(Block::default().borders(Borders::ALL).title(title))
                .scroll((p.scroll, 0));
            f.render_widget(para, popup);
        }
    }
}
