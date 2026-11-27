//! One-line title of an ACP permission request (#1041): the shared summary,
//! as one line. Model- and file-derived text passes the summary's own sanitizer.
use tole_core::action_summary::{ActionEffect, ActionKind, ActionSummary, SEE_FULL};
use tole_core::approval::ToolRequest;
use tole_core::secrets::process_secrets;

/// Whole-title cap: target (core cuts it at 1000) + phrase + effect stay under
/// 1150, so the tool-text detail always keeps 350 and the effect is never cut.
pub const TITLE_CHARS: usize = 1500;

/// `text` as one safe line: `ActionSummary::new` redacts, neutralizes and cuts
/// it at 1000 characters with the TRUE count; `see` replaces core's pointer.
fn line(text: &str, see: &str) -> String {
    let safe = ActionSummary::new(ActionKind::Unknown, text, ActionEffect::Unknown, false);
    safe.target.replace(SEE_FULL, see)
}

/// `<what it does> — <its effect>`; a tool with no summary of its own adds its
/// text (destination, argv) after ` :: `, tail cut (counted) to fit the cap.
/// No summary at all: that text alone, as before.
pub fn permission_title(req: &ToolRequest<'_>) -> String {
    let s = req
        .summary
        .as_ref()
        .map(|s| s.finalized(&process_secrets()));
    let Some(s) = s else {
        return line(&req.description, "not shown");
    };
    let target = line(&s.target, "see rawInput");
    let what = s.kind.text().replace("{target}", &target);
    let mut title = format!("{what} — {}", s.effect.text());
    if s.kind == ActionKind::Unknown {
        let detail = line(&req.description, "not shown");
        let room = TITLE_CHARS.saturating_sub(title.chars().count() + 4);
        let n = detail.chars().count();
        title.push_str(" :: ");
        if n > room {
            let keep = room.saturating_sub(40); // the marker is under 40 characters
            title.extend(detail.chars().take(keep));
            title.push_str(&format!("… ({} more characters, not shown)", n - keep));
        } else {
            title.push_str(&detail);
        }
    }
    title
}
