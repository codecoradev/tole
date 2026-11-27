//! One-line title of an ACP permission request (#1041): the shared summary,
//! cut. Model- and file-derived text passes the summary's own sanitizer.
use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary};
use tole_core::approval::ToolRequest;
use tole_core::secrets::process_secrets;

/// Cut-target, detail and whole-title limits, in characters.
const TARGET: usize = 200;
const DETAIL: usize = 160;
pub const TITLE_CHARS: usize = 600;

/// `text` as one safe line (`ActionSummary::new` redacts and neutralizes it),
/// at most `max` characters; a cut keeps head and tail, never only a head.
fn line(text: &str, max: usize) -> String {
    let s = ActionSummary::new(ActionKind::Unknown, text, ActionEffect::Unknown, false).target;
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let room = max.saturating_sub(32); // the marker is at most 32 characters
    let (head, tail) = (room * 2 / 3, room - room * 2 / 3);
    let mut out: String = s.chars().take(head).collect();
    out.push_str(&format!("… ({} more characters) …", n - head - tail));
    out.extend(s.chars().skip(n - tail));
    out
}

/// `<what it does> — <its effect>`; a tool with no summary of its own adds
/// its text (destination, argv). No summary at all: that text alone, as before.
pub fn permission_title(req: &ToolRequest<'_>) -> String {
    let Some(s) = req
        .summary
        .as_ref()
        .map(|s| s.finalized(&process_secrets()))
    else {
        return line(&req.description, TITLE_CHARS);
    };
    let what = s.kind.text().replace("{target}", &line(&s.target, TARGET));
    let mut title = format!("{what} — {}", s.effect.text());
    if s.kind == ActionKind::Unknown {
        title.push_str(&format!(" :: {}", line(&req.description, DETAIL)));
    }
    title.chars().take(TITLE_CHARS).collect()
}
