//! Rendering (#526): draws the live area from `App` state and styles committed
//! lines. Everything here only READS `App` (`draw` takes `&self`); input,
//! state changes and the terminal loop live elsewhere, so a second front end
//! can reuse the state machine without this module.

use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::theme::{Role, Theme};
use crate::{clip, menu, one_line, App, KeyAction, Kind, EDITOR_ROWS, LIVE_ROWS};

impl App {
    pub fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let th = &self.theme;
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
            .border_style(th.get(Role::Border));
        let edit = block.inner(edit_box);
        f.render_widget(block, edit_box);
        let w = area.width as usize;

        let mut live_lines = Vec::new();
        let menu = self.menu();
        let at = self.at_lines(w);
        if let Some(p) = &self.picker {
            live_lines.extend(
                p.lines(w)
                    .into_iter()
                    .map(|l| Line::styled(l, th.get(Role::Picker))),
            );
        } else if let Some(p) = &self.approval {
            live_lines.push(Line::styled(
                format!("? {}", clip(p.request.text(), w.saturating_sub(2))),
                th.get(Role::Approval),
            ));
            live_lines.push(Line::from("[y] allow  [n] deny  [Esc] deny + cancel"));
        } else if !at.is_empty() {
            live_lines.extend(at.into_iter().map(|l| Line::styled(l, th.get(Role::Menu))));
            live_lines.push(Line::styled(
                "Tab/Enter inserts the path · Up/Down select · keep typing to filter",
                th.get(Role::Hint),
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
                    th.get(Role::Menu),
                ));
            }
            live_lines.push(Line::styled(
                "Tab completes · Up/Down select · Enter runs",
                th.get(Role::Hint),
            ));
        } else {
            let partial = self
                .transcript
                .partial()
                .map(|(k, t)| styled(k, &tail(t, w), th))
                .unwrap_or_default();
            live_lines.push(partial);
            live_lines.extend(
                self.pending_rows(w)
                    .into_iter()
                    .map(|r| Line::styled(r, th.get(Role::Queue))),
            );
            let key = |a| self.keymap.label(a);
            live_lines.push(Line::styled(
                if self.running {
                    format!(
                        "working… Enter steers · {} queues · {} unqueues · Esc cancels",
                        key(KeyAction::QueueFollowUp),
                        key(KeyAction::PopQueue)
                    )
                } else if self.armed {
                    "press Ctrl+C again to quit".to_string()
                } else {
                    format!(
                        "Enter sends · / commands · @ file · {} model · {} thinking · {} newline",
                        key(KeyAction::ModelPicker),
                        key(KeyAction::CycleThinking),
                        key(KeyAction::Newline)
                    )
                },
                th.get(Role::Hint),
            ));
        }
        f.render_widget(Paragraph::new(live_lines), live);

        // A pasted tab would be dropped by the cell renderer (and throw the
        // cursor column off): show it as spaces; the text keeps the tab.
        let lines: Vec<String> = self
            .editor
            .lines()
            .iter()
            .map(|l| l.replace('\t', "    "))
            .collect();
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
            Paragraph::new(Line::styled(self.footer.render(w), th.get(Role::Footer))),
            foot,
        );
    }
}

/// The last `width` chars: the live line shows the newest part of the stream.
pub(crate) fn tail(s: &str, width: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(width)).collect()
}

pub(crate) fn styled(kind: Kind, text: &str, th: &Theme) -> Line<'static> {
    let style = match kind {
        Kind::User => th.get(Role::User),
        Kind::Text => return Line::from(inline_spans(text, th)),
        Kind::Reasoning => th.get(Role::Reasoning),
        Kind::Tool => th.get(Role::Tool),
        Kind::ToolOut => diff_style(text, th),
        Kind::Heading => th.get(Role::Heading),
        Kind::Code => th.get(Role::Code),
        Kind::Note => th.get(Role::Note),
    };
    Line::from(Span::styled(text.to_string(), style))
}

/// Unified-diff rows in a tool result: `+` add, `-` remove, `@@`/headers
/// hunk. The text is already sanitized by the transcript.
pub(crate) fn diff_style(text: &str, th: &Theme) -> Style {
    let t = text.trim_start();
    if t.starts_with("+++") || t.starts_with("---") || t.starts_with("@@") {
        th.get(Role::DiffHunk)
    } else if t.starts_with('+') {
        th.get(Role::DiffAdd)
    } else if t.starts_with('-') {
        th.get(Role::DiffRemove)
    } else {
        th.get(Role::ToolOutput)
    }
}

/// Minimal inline markdown: `**bold**` and `` `code` ``; an unclosed marker
/// stays literal. (Not a renderer: no italics, links, lists or tables.)
pub(crate) fn inline_spans(text: &str, th: &Theme) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let (open, style) = if rest.starts_with("**") {
            ("**", th.get(Role::InlineBold))
        } else if c == '`' {
            ("`", th.get(Role::InlineCode))
        } else {
            plain.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        };
        match rest[open.len()..].find(open) {
            Some(end) if end > 0 => {
                if !plain.is_empty() {
                    out.push(Span::styled(std::mem::take(&mut plain), th.get(Role::Text)));
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
        out.push(Span::styled(plain, th.get(Role::Text)));
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
