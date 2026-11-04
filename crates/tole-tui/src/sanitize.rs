//! Terminal-injection guard for every string that reaches the screen.
//!
//! ratatui/crossterm write cell text verbatim, so ESC/CSI/CR/C1 in tool
//! output, provider errors or model ids would be interpreted by the terminal
//! (spoofed approval prompt, cleared frame). Same policy as tole-cli's
//! `config_trust::sanitize` (tole-core's twin is crate-private): every unsafe
//! char becomes a visible `\u{hex}`, TAB becomes `\t`.

fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

fn clean(s: &str, newline: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\n' => out.push_str(newline),
            '\t' => out.push_str("\\t"),
            c if is_unsafe(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Multi-line context (transcript): `\n` is kept, wrap/commit split on it.
pub(crate) fn sanitize(s: &str) -> String {
    clean(s, "\n")
}

/// Single-line context (tool lines, approval, footer): `\n` becomes ` ⏎ `.
pub(crate) fn one_line(s: &str) -> String {
    clean(s, " ⏎ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_chars_become_visible_escapes() {
        for (raw, want) in [
            ("\x1b[2J", "\\u{1b}[2J"),
            ("a\rb", "a\\u{d}b"),
            ("\u{9b}", "\\u{9b}"),
            ("\0", "\\u{0}"),
            ("a\tb", "a\\tb"),
            ("\u{202e}", "\\u{202e}"),
        ] {
            assert_eq!(sanitize(raw), want);
            assert_eq!(one_line(raw), want);
        }
    }

    #[test]
    fn newline_kept_in_transcript_flattened_in_one_line() {
        assert_eq!(sanitize("a\nb"), "a\nb");
        assert_eq!(one_line("a\nb"), "a ⏎ b");
    }

    #[test]
    fn plain_text_is_untouched() {
        let s = "héllo wörld 日本 🙂 — ok";
        assert_eq!(sanitize(s), s);
        assert_eq!(one_line(s), s);
    }
}
