//! The Connected tools tab: one card per server with its trust state.
//!
//! The trust word and its colour are the model's (`ToolTrust`, `trust_tone`):
//! only a trusted tool is green, one that changed or was never trusted is amber
//! and says so in words, and a removed one is quiet. The state is the saved
//! approval, not a live check, and the page says that too. Nothing here trusts
//! a tool.

use super::cards::{card, mono, small, title_row};
use super::parts::{caption, line, plain};
use super::words;
use crate::ui::adapter::*;
use crate::ui::shell::ShellView;
use tole_desktop_model::tokens::{font, size, Tone, DARK};
use tole_desktop_model::{trust_tone, ToolRow, ToolTrust};

/// The marker and the words of a trust state. Filled for a tool Tole can use or
/// that changed, hollow for one it does not use.
fn trust_cell(tool: &ToolRow) -> impl IntoElement {
    let colour = color(DARK.tone(trust_tone(tool.trust)));
    let marker = div()
        .size(len(size::DOT))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(colour);
    let marker = if matches!(tool.trust, ToolTrust::Trusted | ToolTrust::Changed) {
        marker.bg(colour)
    } else {
        marker
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(len(size::GUTTER))
        .text_color(colour)
        .child(marker)
        .child(words::trust_label(tool.trust))
}

fn tool_card(tool: &ToolRow, probed: bool) -> impl IntoElement {
    let mut rows = vec![
        title_row(&tool.name, trust_cell(tool)).into_any_element(),
        line(words::trust_line(tool.trust, probed)).into_any_element(),
    ];
    if let Some(change) = &tool.change {
        rows.push(small(&words::change_text(change), Tone::Attention).into_any_element());
    }
    rows.push(caption(&words::tool_details(tool)).into_any_element());
    rows.push(mono(&tool.state).into_any_element());
    card(
        matches!(tool.trust, ToolTrust::Changed | ToolTrust::NotTrusted),
        rows,
    )
}

/// The column titles, over the cards.
fn columns() -> impl IntoElement {
    let [tool, trust] = words::TOOL_COLUMNS;
    div()
        .flex()
        .justify_between()
        .px(len(size::CARD_PADDING))
        .text_size(len(font::CAPTION))
        .text_color(color(DARK.text_faint))
        .child(tool)
        .child(trust)
}

impl ShellView {
    /// The servers, or the empty line when the model says the answer was
    /// complete and there are none: a partial read never says "none".
    pub(super) fn tool_cards(&self) -> impl IntoElement {
        let page = &self.skills.page;
        let mut rows = Vec::new();
        // Said first, above the cards: the state is the saved approval.
        if !page.tools_probed {
            rows.push(caption(words::TOOLS_LIVE_CHECK).into_any_element());
        }
        if page.no_connected_tools() {
            rows.push(plain(words::TOOLS_EMPTY).into_any_element());
        } else if !page.tools.is_empty() {
            rows.push(columns().into_any_element());
            rows.extend(
                page.tools
                    .iter()
                    .map(|t| tool_card(t, page.tools_probed).into_any_element()),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(len(size::GUTTER))
            .children(rows)
    }
}
