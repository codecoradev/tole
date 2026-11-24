//! An answer's Markdown as elements (`tole_desktop_model::markdown`).

use crate::ui::adapter::*;
use std::cell::RefCell;
use std::sync::Arc;

use tole_desktop_model::markdown::{Block, ListItem, Span};
use tole_desktop_model::markdown_cache::Cache;
use tole_desktop_model::task_strings as words;
use tole_desktop_model::tokens::{font, size, DARK};

thread_local! {
    /// The answers drawn lately, parsed: a repaint of the same text does not
    /// parse it again. The window draws on one thread.
    static PARSED: RefCell<Cache> = RefCell::new(Cache::default());
}

fn blocks_of(source: &str) -> Arc<Vec<Block>> {
    PARSED.with(|cache| cache.borrow_mut().blocks(source))
}

/// The spans of a line as one wrapping text. `base` is the colour of plain text.
fn text(spans: &[Span], base: tole_desktop_model::tokens::Rgb) -> AnyElement {
    let p = DARK;
    let runs: Vec<Run<'_>> = (spans.iter())
        .map(|s| Run {
            text: &s.text,
            bold: s.style.bold,
            italic: s.style.italic,
            fill: Some(if s.style.code { p.text_primary } else { base }),
            back: s.style.code.then_some(p.raised),
        })
        .collect();
    div().child(rich_text(&runs)).into_any_element()
}

fn item(item: &ListItem) -> AnyElement {
    let p = DARK;
    let marker = item
        .number
        .map_or_else(|| words::BULLET.to_owned(), words::list_number);
    div()
        .flex()
        .pl(len(size::INDENT * f32::from(item.depth)))
        .child(
            div()
                .w(len(size::MARKER_WIDTH))
                .flex_none()
                .text_color(color(p.text_muted))
                .child(marker),
        )
        .child(
            div()
                .flex_1()
                .min_w(len(0.0))
                .child(text(&item.spans, p.text_primary)),
        )
        .into_any_element()
}

fn block(block: &Block) -> AnyElement {
    let p = DARK;
    match block {
        Block::Paragraph(spans) => text(spans, p.text_primary),
        Block::Heading { level, spans } => {
            let heading = div()
                .text_size(len(font::HEADING))
                .font_weight(FontWeight::SEMIBOLD);
            let heading = if *level > 2 {
                heading.text_size(len(font::BODY))
            } else {
                heading
            };
            heading
                .child(text(spans, p.text_primary))
                .into_any_element()
        }
        Block::List(items) => div()
            .flex()
            .flex_col()
            .gap(len(size::ROW_GAP))
            .children(items.iter().map(item))
            .into_any_element(),
        Block::Code(code) => div()
            .px(len(size::BUBBLE_PAD_X))
            .py(len(size::BUBBLE_PAD_Y))
            .rounded(len(size::RADIUS))
            .bg(color(p.raised))
            .font_family(font::FAMILY_MONO)
            .text_size(len(font::SMALL))
            .text_color(color(p.text_primary))
            .child(code.clone())
            .into_any_element(),
        Block::Quote(spans) => div()
            .pl(len(size::INDENT))
            .border_l_2()
            .border_color(color(p.line))
            .child(text(spans, p.text_muted))
            .into_any_element(),
        Block::Rule => div()
            .h(len(size::HAIRLINE))
            .bg(color(p.line))
            .into_any_element(),
    }
}

/// The answer, block by block; the last line says when it was cut.
pub(super) fn answer(source: &str, cut: bool) -> AnyElement {
    let p = DARK;
    div()
        .flex()
        .flex_col()
        .gap(len(size::GUTTER))
        .line_height(rel(font::LINE))
        .text_color(color(p.text_primary))
        .children(blocks_of(source).iter().map(block))
        .children(cut.then(|| {
            div()
                .text_size(len(font::SMALL))
                .text_color(color(p.text_faint))
                .child(words::ANSWER_CUT)
        }))
        .into_any_element()
}
