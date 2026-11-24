//! The preview of a card: the diff of a file, or the command to run.

use super::super::adapter::*;
use tole_desktop_model::approvals::{strings, LineKind, Preview, PreviewLine, Shape};
use tole_desktop_model::tokens::{font, size, tint, DARK};

/// The lines of `preview`: a file's diff with line numbers and green and red
/// lines, or text as it is, in the monospace font. A cut marker of the server
/// is drawn as "9 more lines", never as content.
pub fn preview_block(preview: &Preview) -> impl IntoElement {
    let p = DARK;
    let mut number = 0;
    let rows: Vec<_> = preview
        .lines
        .iter()
        .map(|line| {
            if !line.kind.is_cut() {
                number += 1;
            }
            line_row(line, preview.shape, number).into_any_element()
        })
        .collect();
    div()
        .id("approval-preview")
        .w_full()
        .max_h(len(size::PREVIEW_MAX_HEIGHT))
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .rounded(len(size::RADIUS))
        .border_1()
        .border_color(color(p.line))
        .bg(color(p.panel))
        .font_family(font::FAMILY_MONO)
        .text_size(len(font::BODY))
        .children(rows)
}

fn line_row(line: &PreviewLine, shape: Shape, number: usize) -> impl IntoElement {
    let p = DARK;
    let (mark, fill, ink) = match line.kind {
        LineKind::Added => ("+", Some(tint::ADDED), p.text_primary),
        LineKind::Removed => ("-", Some(tint::REMOVED), p.text_primary),
        LineKind::Plain => (" ", None, p.text_primary),
        LineKind::CutAdded | LineKind::CutRemoved | LineKind::Cut => (" ", None, p.text_faint),
    };
    let cut = line.kind.is_cut();
    let text = match cut {
        true => strings::more_lines(line.omitted),
        false => line.text.clone(),
    };
    let numbered = shape == Shape::Diff;
    let gutter = div()
        .w(len(size::LINE_NUMBER_WIDTH))
        .flex_none()
        .text_color(color(p.text_faint))
        .child(match (numbered, cut) {
            (true, false) => number.to_string(),
            _ => String::new(),
        });
    let row = div()
        .w_full()
        .flex()
        .gap(len(size::GUTTER))
        .px(len(size::GUTTER))
        .when(numbered, |row| row.child(gutter))
        .when(numbered, |row| {
            row.child(div().w(len(size::GUTTER)).flex_none().child(mark))
        })
        .child(
            div()
                .flex_1()
                .min_w(len(0.0))
                .text_color(color(ink))
                .child(text),
        );
    match fill {
        Some(fill) => row.bg(color(fill)),
        None => row,
    }
}
