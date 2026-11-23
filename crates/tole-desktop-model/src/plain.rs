//! Small pieces shared by the list pages (Automations, Skills and tools).

/// Where a page's data is: nothing asked yet, asked, shown, or the last ask
/// failed (with the words to show).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Load {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// Characters that hide or reorder text: bidirectional marks, overrides and
/// isolates, zero-width characters, the byte order mark.
fn hidden(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'
            | '\u{2066}'..='\u{2069}'
            | '\u{061C}'
            | '\u{FEFF}'
    )
}

/// Long model text (an answer, in Markdown) made safe to draw without losing
/// its structure: what [`printable`] drops is dropped, the line and paragraph
/// separators and other line breaks stay as `\n` (`\r` is dropped, so `\r\n`
/// is one break), tabs stay, other control characters become spaces. It works
/// character by character, so a text cut into pieces cleans the same.
pub(crate) fn clean_text(s: &str) -> String {
    s.chars()
        .filter(|c| !hidden(*c) && *c != '\r')
        .map(|c| match c {
            '\u{2028}' | '\u{2029}' => '\n',
            '\n' | '\t' => c,
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

/// [`clean_text`] cut to `max` characters (the last one an ellipsis when cut).
/// The flag says whether it was cut.
pub(crate) fn printable_text(s: &str, max: usize) -> (String, bool) {
    let clean = clean_text(s);
    if clean.chars().count() <= max {
        return (clean, false);
    }
    let mut cut: String = clean.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    (cut, true)
}

/// Text that came from a file, a model or a server, made safe to draw: control
/// characters (newlines, tabs, escapes) and the line and paragraph separators
/// become spaces; bidirectional marks and overrides and zero-width characters
/// are dropped; the ends are trimmed and it is cut to `max` characters (an
/// ellipsis marks a cut). Nothing a model wrote is shown raw. Public so that an
/// adapter that must hand a server's words to a page outside the model's own
/// text (an error message) cleans them the same way.
pub fn printable(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .filter(|c| !hidden(*c))
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_owned();
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_and_overrides_never_reach_the_screen() {
        assert_eq!(printable("a\nb\t\u{1b}[31mc", 40), "a b  [31mc");
        assert_eq!(printable("x\u{202E}evil", 40), "xevil");
        assert_eq!(printable("  padded  ", 40), "padded");
    }

    #[test]
    fn marks_that_hide_or_reorder_text_are_dropped_and_separators_are_spaces() {
        assert_eq!(printable("a\u{200E}b\u{200F}c\u{061C}d", 40), "abcd");
        assert_eq!(
            printable("a\u{200B}b\u{200D}c\u{2060}d\u{FEFF}e", 40),
            "abcde"
        );
        assert_eq!(
            printable("one\u{2028}two\u{2029}three", 40),
            "one two three"
        );
    }

    #[test]
    fn a_long_text_is_cut_with_a_mark() {
        let cut = printable(&"a".repeat(50), 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('\u{2026}'));
        assert_eq!(printable("short", 10), "short");
    }

    #[test]
    fn long_text_keeps_its_lines_loses_what_hides_text_and_is_cut_at_a_character() {
        let clean = clean_text("a\r\nb\u{2028}c\td\u{202E}e\u{1b}f\u{200B}");
        assert_eq!(clean, "a\nb\nc\tde f");
        assert_eq!(printable_text("héllo", 10), ("héllo".to_string(), false));
        assert_eq!(
            printable_text("héllo wörld", 5),
            ("héll\u{2026}".to_string(), true)
        );
    }
}
