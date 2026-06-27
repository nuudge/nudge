//! CommonMark → styled ratatui lines. pulldown-cmark handles parsing; this module
//! owns only the event→Span mapping. Output is ready for the log paragraph —
//! `render_log` adds the turn prefix, so nothing here knows about markers or width.

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// Code accent, kept distinct from body text.
const CODE_FG: Color = Color::Yellow;
const HEADING_FG: Color = Color::Cyan;
const QUOTE_FG: Color = Color::DarkGray;
const RULE: &str = "────────────────────";

struct Renderer {
    out: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    bold: u32,
    italic: u32,
    strike: u32,
    in_code_block: bool,
    heading: Option<HeadingLevel>,
    // One entry per open list: Some(next_number) for ordered, None for bullet.
    lists: Vec<Option<u64>>,
    in_quote: u32,
    // Marker emitted at the next line start; deferred so loose lists (item wrapped
    // in a paragraph) still get it.
    pending_marker: Option<String>,
    // Buffered: column widths need every row, so cells accumulate and the grid emits
    // on the closing tag. Cell styling is flattened to plain text.
    table: Option<Table>,
}

struct Table {
    aligns: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cur_cell: String,
    in_head: bool,
}

impl Renderer {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            cur: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            in_code_block: false,
            heading: None,
            lists: Vec::new(),
            in_quote: 0,
            pending_marker: None,
            table: None,
        }
    }

    fn line_prefix(&self) -> String {
        let mut p = String::new();
        for _ in 0..self.in_quote {
            p.push_str("│ ");
        }
        for _ in 0..self.lists.len().saturating_sub(1) {
            p.push_str("  ");
        }
        p
    }

    fn inline_style(&self) -> Style {
        let mut s = Style::default();
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.in_quote > 0 {
            s = s.fg(QUOTE_FG);
        }
        if let Some(level) = self.heading {
            s = s.fg(HEADING_FG).add_modifier(Modifier::BOLD);
            if level == HeadingLevel::H1 {
                s = s.add_modifier(Modifier::UNDERLINED);
            }
        }
        s
    }

    // Emit the line's indent/quote-bar/marker before the first content span.
    // Idempotent: only fires when `cur` is empty.
    fn ensure_line_start(&mut self) {
        if !self.cur.is_empty() {
            return;
        }
        let prefix = self.line_prefix();
        if !prefix.is_empty() {
            let style = if self.in_quote > 0 {
                Style::default().fg(QUOTE_FG)
            } else {
                Style::default()
            };
            self.cur.push(Span::styled(prefix, style));
        }
        if let Some(marker) = self.pending_marker.take() {
            self.cur
                .push(Span::styled(marker, Style::default().fg(HEADING_FG)));
        }
    }

    fn push_span(&mut self, text: String, style: Style) {
        if text.is_empty() {
            return;
        }
        self.ensure_line_start();
        self.cur.push(Span::styled(text, style));
    }

    // Flush spans as a line; an empty buffer becomes a blank row (paragraph/block break).
    fn flush(&mut self) {
        if self.cur.is_empty() {
            self.out.push(Line::from(""));
        } else {
            self.out.push(Line::from(std::mem::take(&mut self.cur)));
        }
    }

    // Flush only if a line is in progress (no manufactured blank row).
    fn flush_soft(&mut self) {
        if !self.cur.is_empty() {
            self.out.push(Line::from(std::mem::take(&mut self.cur)));
        }
    }

    fn handle(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(table) = &mut self.table {
                    table.cur_cell.push_str(&sanitize(&t));
                } else if self.in_code_block {
                    self.emit_code_lines(&t);
                } else {
                    let style = self.inline_style();
                    self.push_span(sanitize(&t), style);
                }
            }
            Event::Code(t) => {
                if let Some(table) = &mut self.table {
                    table.cur_cell.push_str(&sanitize(&t));
                } else {
                    self.push_span(sanitize(&t), Style::default().fg(CODE_FG));
                }
            }
            // Soft break (source newline in a paragraph) → space; the outer Paragraph
            // re-wraps. Hard break → flush.
            Event::SoftBreak => {
                if let Some(table) = &mut self.table {
                    table.cur_cell.push(' ');
                } else {
                    let style = self.inline_style();
                    self.push_span(" ".into(), style);
                }
            }
            Event::HardBreak => self.flush_soft(),
            Event::Rule => {
                self.flush_soft();
                self.out.push(Line::from(Span::styled(
                    RULE,
                    Style::default().fg(QUOTE_FG),
                )));
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                self.push_span(mark.into(), Style::default().fg(HEADING_FG));
            }
            // Render raw HTML/math text rather than dropping it.
            Event::Html(t) | Event::InlineHtml(t) => {
                let style = self.inline_style();
                self.push_span(sanitize(&t), style);
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => {
                self.push_span(sanitize(&t), Style::default().fg(CODE_FG));
            }
            Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.flush_soft(),
            Tag::Heading { level, .. } => {
                self.flush_soft();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_soft();
                self.in_quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush_soft();
                self.in_code_block = true;
            }
            Tag::List(start) => {
                self.flush_soft();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush_soft();
                // Marker from the innermost list; advance the ordered counter.
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.pending_marker = Some(marker);
            }
            Tag::Table(aligns) => {
                self.flush_soft();
                self.table = Some(Table {
                    aligns,
                    header: Vec::new(),
                    rows: Vec::new(),
                    cur_row: Vec::new(),
                    cur_cell: String::new(),
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.cur_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur_row.clear();
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cur_cell.clear();
                }
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            // Links/images: keep the visible text (child Text events); drop the URL.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush_soft();
                self.heading = None;
                self.out.push(Line::from(""));
            }
            TagEnd::BlockQuote(_) => {
                self.flush_soft();
                self.in_quote = self.in_quote.saturating_sub(1);
                self.out.push(Line::from(""));
            }
            TagEnd::CodeBlock => {
                self.flush_soft();
                self.in_code_block = false;
                self.out.push(Line::from(""));
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.out.push(Line::from(""));
                }
            }
            TagEnd::Item => self.flush_soft(),
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = std::mem::take(&mut t.cur_cell).trim().to_string();
                    t.cur_row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.header = std::mem::take(&mut t.cur_row);
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table
                    && !t.in_head
                {
                    let row = std::mem::take(&mut t.cur_row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            _ => {}
        }
    }

    // One Text event spans multiple lines; split so each becomes its own indented row.
    fn emit_code_lines(&mut self, text: &str) {
        let style = Style::default().fg(CODE_FG);
        // Drop the block's trailing newline so there's no spurious blank code row.
        for line in text.trim_end_matches('\n').split('\n') {
            self.out.push(Line::from(Span::styled(
                format!("  {}", sanitize(line)),
                style,
            )));
        }
    }

    // Columns sized to the widest cell; bold header + separator. Wide tables wrap
    // rather than truncate (truncation loses more in a log view).
    fn render_table(&mut self, t: Table) {
        let cols = t
            .aligns
            .len()
            .max(t.header.len())
            .max(t.rows.iter().map(Vec::len).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let mut widths = vec![0usize; cols];
        let mut measure = |row: &[String]| {
            for (i, c) in row.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        };
        measure(&t.header);
        for row in &t.rows {
            measure(row);
        }

        let prefix = self.line_prefix();
        if !t.header.is_empty() {
            self.out.push(Line::from(Span::styled(
                format!("{prefix}{}", row_string(&t.header, &widths, &t.aligns)),
                Style::default().fg(HEADING_FG).add_modifier(Modifier::BOLD),
            )));
            let sep = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("─┼─");
            self.out.push(Line::from(Span::styled(
                format!("{prefix}{sep}"),
                Style::default().fg(QUOTE_FG),
            )));
        }
        for row in &t.rows {
            self.out.push(Line::from(format!(
                "{prefix}{}",
                row_string(row, &widths, &t.aligns)
            )));
        }
        self.out.push(Line::from(""));
    }
}

fn row_string(cells: &[String], widths: &[usize], aligns: &[Alignment]) -> String {
    let mut parts = Vec::with_capacity(widths.len());
    for (i, w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        let align = aligns.get(i).copied().unwrap_or(Alignment::None);
        parts.push(pad_cell(cell, *w, align));
    }
    parts.join(" │ ")
}

fn pad_cell(s: &str, width: usize, align: Alignment) -> String {
    let len = s.chars().count();
    let pad = width.saturating_sub(len);
    match align {
        Alignment::Right => format!("{}{s}", " ".repeat(pad)),
        Alignment::Center => {
            let left = pad / 2;
            format!("{}{s}{}", " ".repeat(left), " ".repeat(pad - left))
        }
        _ => format!("{s}{}", " ".repeat(pad)),
    }
}

// Same control-char defense as the TUI's sanitize_span_text — ratatui garbles a
// literal tab/newline/control char on repaint.
fn sanitize(s: &str) -> String {
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

/// Parse `text` as CommonMark and render to styled lines, trimming the trailing
/// blank so the caller's inter-entry spacing isn't doubled.
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);

    let mut r = Renderer::new();
    for ev in Parser::new_ext(text, opts) {
        r.handle(ev);
    }
    r.flush_soft();
    while matches!(r.out.last(), Some(l) if line_is_blank(l)) {
        r.out.pop();
    }
    r.out
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}
