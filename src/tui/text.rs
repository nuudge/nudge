use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::EXPANDED_MAX_LINES;

// Marker on the first line, 2-space continuation indent on the rest.
pub(super) fn emit_prefixed(
    out: &mut Vec<Line<'static>>,
    text: &str,
    marker: &str,
    marker_style: Style,
) {
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
        out.push(Line::from(Span::styled(marker.to_string(), marker_style)));
    }
}

// Like emit_prefixed but for already-styled (markdown-rendered) lines.
pub(super) fn emit_prefixed_lines(
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

// ratatui copies each char into a cell verbatim, so a literal tab/newline/control
// char would garble later cells on repaint. All foreign text passes through here.
pub(super) fn sanitize_span_text(s: &str) -> String {
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

// First non-empty line of a block, trimmed.
pub(super) fn first_content_line(content: &str) -> &str {
    content
        .lines()
        .map(str::trim_end)
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
}

pub(super) fn truncate_chars(s: &str, max: usize) -> String {
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

pub(super) fn emit_expanded_thinking(out: &mut Vec<Line<'static>>, content: &str, style: Style) {
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

// Single `  ⎿ <preview>` row, hard-truncated so it never wraps. With preview=false,
// multi-line output collapses to a bare line count (positional-noise tools); a
// single-line output is always shown verbatim.
pub(super) fn emit_collapsed_result(
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
        // Single-line output that doesn't fit — the hint becomes the "there's more" signal.
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

pub(super) fn emit_expanded_result(
    out: &mut Vec<Line<'static>>,
    content: &str,
    elbow_style: Style,
) {
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

pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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
