//! Crate-internal text sanitizer for approval surfaces (#286, #327).
//!
//! Model- or server-controlled strings reach human-facing approval
//! prompts (the `what:` line, the ACP permission title, the serve
//! queue). A raw ESC, CR, bidi override or line separator in them can
//! rewrite or visually reorder the prompt and make it say something
//! other than what will execute.

/// True for characters that must never reach an approval surface raw:
/// C0 controls (incl. ESC, CR, LF, TAB), DEL, C1 controls, line/paragraph
/// separators, bidi marks/overrides/isolates and zero-width invisibles.
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// Make `s` safe for a single-line approval prompt.
///
/// Policy: every unsafe character (see `is_unsafe`) is replaced by a
/// VISIBLE escape `\u{HEX}` (lowercase hex, no padding, e.g. ESC becomes
/// `\u{1b}`, CR `\u{d}`, RLO `\u{202e}`), so a reviewer can see that
/// something was there. The output contains no control or invisible
/// characters; everything else (spaces, quotes, letters of any script,
/// emoji) is passed through unchanged. Does NOT truncate. Idempotent.
pub(crate) fn sanitize_one_line(s: &str) -> String {
    if !s.chars().any(is_unsafe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if is_unsafe(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_each_unsafe_class_visibly() {
        for (raw, want) in [
            ("\x1b", "\\u{1b}"),
            ("\r", "\\u{d}"),
            ("\n", "\\u{a}"),
            ("\t", "\\u{9}"),
            ("\0", "\\u{0}"),
            ("\x7f", "\\u{7f}"),
            ("\u{80}", "\\u{80}"),
            ("\u{9f}", "\\u{9f}"),
            ("\u{202A}", "\\u{202a}"),
            ("\u{202E}", "\\u{202e}"),
            ("\u{2066}", "\\u{2066}"),
            ("\u{2069}", "\\u{2069}"),
            ("\u{200E}", "\\u{200e}"),
            ("\u{200F}", "\\u{200f}"),
            ("\u{2028}", "\\u{2028}"),
            ("\u{2029}", "\\u{2029}"),
            ("\u{200B}", "\\u{200b}"),
            ("\u{200C}", "\\u{200c}"),
            ("\u{200D}", "\\u{200d}"),
            ("\u{FEFF}", "\\u{feff}"),
        ] {
            assert_eq!(sanitize_one_line(&format!("a{raw}b")), format!("a{want}b"));
        }
    }

    #[test]
    fn ansi_sequence_loses_its_escape() {
        let out = sanitize_one_line("x\x1b[2Ky");
        assert_eq!(out, "x\\u{1b}[2Ky");
        assert!(!out.chars().any(|c| c.is_control()));
    }

    #[test]
    fn benign_text_is_unchanged() {
        let s = "write file \"a b/é.txt\" 日本語 🚀 it's ok";
        assert_eq!(sanitize_one_line(s), s);
    }

    #[test]
    fn idempotent_and_does_not_truncate() {
        let long = format!("{}\x1b", "a".repeat(5000));
        let once = sanitize_one_line(&long);
        assert_eq!(sanitize_one_line(&once), once);
        assert!(once.len() > 5000);
    }
}
