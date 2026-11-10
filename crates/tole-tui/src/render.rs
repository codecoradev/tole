//! Rendering (#526): draws the live area from `App` state and styles committed
//! lines. Everything here only READS `App` (`draw` takes `&self`); input,
//! state changes and the terminal loop live elsewhere, so a second front end
//! can reuse the state machine without this module.

use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::{clip, menu, one_line, App, Kind, EDITOR_ROWS, LIVE_ROWS};

impl App {
    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let [live, edit_box, foot] = Layout::vertical([
            Constraint::Length(LIVE_ROWS),
            Constraint::Length(EDITOR_ROWS as u16 + 1),
            Constraint::Length(1),
        ])
        .areas(area);
        // The border shows the session's reasoning level (#461).
        let level = self.footer.effort.as_deref().unwrap_or("-");
        let block = Block::new()
            .borders(Borders::TOP)
            .title(one_line(&format!(" thinking: {level} ")))
            .border_style(Style::new().add_modifier(Modifier::DIM));
        let edit = block.inner(edit_box);
        f.render_widget(block, edit_box);
        let w = area.width as usize;

        let mut live_lines = Vec::new();
        let menu = self.menu();
        let at = self.at_lines(w);
        if let Some(p) = &self.picker {
            live_lines.extend(p.lines(w).into_iter().map(Line::from));
        } else if let Some(p) = &self.approval {
            live_lines.push(Line::styled(
                format!("? {}", clip(p.request.text(), w.saturating_sub(2))),
                Style::new().fg(Color::Yellow),
            ));
            live_lines.push(Line::from("[y] allow  [n] deny  [Esc] deny + cancel"));
        } else if !at.is_empty() {
            live_lines.extend(
                at.into_iter()
                    .map(|l| Line::styled(l, Style::new().fg(Color::Cyan))),
            );
            live_lines.push(Line::styled(
                "Tab/Enter inserts the path · Up/Down select · keep typing to filter",
                Style::new().add_modifier(Modifier::DIM),
            ));
        } else if !menu.is_empty() {
            let sel = self.menu_sel.min(menu.len() - 1);
            for (i, name) in menu.iter().enumerate().take(LIVE_ROWS as usize) {
                let help = menu::COMMANDS
                    .iter()
                    .find(|c| c.0 == *name)
                    .map_or("", |c| c.1);
                let mark = if i == sel { "▸ " } else { "  " };
                live_lines.push(Line::styled(
                    clip(&format!("{mark}{name}  {help}"), w),
                    Style::new().fg(Color::Cyan),
                ));
            }
            live_lines.push(Line::styled(
                "Tab completes · Up/Down select · Enter runs",
                Style::new().add_modifier(Modifier::DIM),
            ));
        } else {
            let partial = self
                .transcript
                .partial()
                .map(|(k, t)| styled(k, &tail(t, w)))
                .unwrap_or_default();
            live_lines.push(partial);
            live_lines.extend(
                self.queue_rows(w)
                    .into_iter()
                    .map(|r| Line::styled(r, Style::new().fg(Color::Magenta))),
            );
            live_lines.push(Line::styled(
                if self.running {
                    "working… Enter steers · Alt+Enter queues · Alt+Up unqueues · Esc cancels"
                } else if self.armed {
                    "press Ctrl+C again to quit"
                } else {
                    "Enter sends · / commands · @ file · Ctrl+L model · Shift+Tab thinking · Shift+Enter newline"
                },
                Style::new().add_modifier(Modifier::DIM),
            ));
        }
        f.render_widget(Paragraph::new(live_lines), live);

        let lines = self.editor.lines();
        let shown = &lines[lines.len().saturating_sub(EDITOR_ROWS)..];
        let first = lines.len() - shown.len();
        let rows: Vec<Line> = shown
            .iter()
            .enumerate()
            .map(|(i, l)| Line::from(format!("{}{l}", if first + i == 0 { "> " } else { "  " })))
            .collect();
        f.render_widget(Paragraph::new(rows), edit);
        let last = shown.last().map_or(0, |l| l.chars().count()) + 2;
        f.set_cursor_position(Position::new(
            edit.x + (last as u16).min(edit.width.saturating_sub(1)),
            edit.y + shown.len().saturating_sub(1) as u16,
        ));

        f.render_widget(
            Paragraph::new(Line::styled(
                self.footer.render(w),
                Style::new().add_modifier(Modifier::DIM),
            )),
            foot,
        );
    }
}

/// The last `width` chars: the live line shows the newest part of the stream.
pub(crate) fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(width)).collect()
}

pub(crate) fn styled(kind: Kind, text: &str) -> Line<'static> {
    let style = match kind {
        Kind::User => Style::new().add_modifier(Modifier::BOLD),
        Kind::Text => Style::new(),
        Kind::Reasoning => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        Kind::Tool => Style::new().fg(Color::Cyan),
        Kind::ToolOut => diff_style(text),
        Kind::Heading => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        Kind::Code => Style::new().add_modifier(Modifier::DIM),
        Kind::Note => Style::new().fg(Color::Yellow),
    };
    if kind == Kind::Text {
        return Line::from(inline_spans(text));
    }
    Line::from(Span::styled(text.to_string(), style))
}

/// Unified-diff rows in a tool result: `+` green, `-` red, `@@`/headers dim.
/// The text is already sanitized by the transcript.
pub(crate) fn diff_style(text: &str) -> Style {
    let t = text.trim_start();
    if t.starts_with("+++") || t.starts_with("---") || t.starts_with("@@") {
        Style::new().add_modifier(Modifier::DIM)
    } else if t.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if t.starts_with('-') {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::DIM)
    }
}

/// Minimal inline markdown: `**bold**` and `` `code` ``; an unclosed marker
/// stays literal. (Not a renderer: no italics, links, lists or tables.)
pub(crate) fn inline_spans(text: &str) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let (open, style) = if rest.starts_with("**") {
            ("**", Style::new().add_modifier(Modifier::BOLD))
        } else if c == '`' {
            ("`", Style::new().fg(Color::Cyan))
        } else {
            plain.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        };
        match rest[open.len()..].find(open) {
            Some(end) if end > 0 => {
                if !plain.is_empty() {
                    out.push(Span::raw(std::mem::take(&mut plain)));
                }
                out.push(Span::styled(
                    rest[open.len()..open.len() + end].to_string(),
                    style,
                ));
                rest = &rest[open.len() * 2 + end..];
            }
            _ => {
                plain.push_str(open);
                rest = &rest[open.len()..];
            }
        }
    }
    if !plain.is_empty() {
        out.push(Span::raw(plain));
    }
    out
}

/// Wrap a committed line to `width` chars per row (char count, not cell width).
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(width).map(|c| c.iter().collect()).collect()
}
