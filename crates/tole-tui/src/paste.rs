//! Pasted text (#560): normalizing a pasted block before it reaches the
//! editor, and recognizing a paste that is exactly one image reference.

/// Extensions of the image types tole can attach (the host sniffs the bytes;
/// this only decides whether a paste is worth offering to the host).
const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// CRLF and lone CR become LF (terminals send CR for a pasted line break);
/// every other control character except newline and tab is dropped. The
/// editor text is drawn verbatim, so nothing that could steer the terminal
/// (ESC, C1) may stay in it — the same chars the `sanitize` module escapes
/// on display.
pub(crate) fn normalize(raw: &str) -> String {
    let unified = raw.replace("\r\n", "\n").replace('\r', "\n");
    unified
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// The image spec a paste stands for, if the WHOLE paste is one image
/// reference: a `data:image/…` URL, or a single path with an image extension
/// (`.png .jpg .jpeg .gif .webp`, any case). A path may be wrapped in single
/// or double quotes, or have its spaces escaped as `\ ` (what terminals do
/// for a dropped file). Anything else — several words, several lines — is
/// `None` and gets inserted as text. Whether the file exists and lies inside
/// the workspace jail is the host's decision, not this one.
pub(crate) fn image_spec(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    if t.len() > 5 && t[..5].eq_ignore_ascii_case("data:") {
        return t[5..]
            .to_ascii_lowercase()
            .starts_with("image/")
            .then(|| t.to_string());
    }
    let path = unquote(t)?;
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    IMAGE_EXTS.contains(&ext.as_str()).then_some(path)
}

/// One path from a paste: `'a b.png'`, `"a b.png"`, `a\ b.png` or `a.png`.
/// `None` when it is more than one word.
fn unquote(t: &str) -> Option<String> {
    for q in ['\'', '"'] {
        if let Some(inner) = t.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return (!inner.is_empty() && !inner.contains(q)).then(|| inner.to_string());
        }
        if t.starts_with(q) || t.ends_with(q) {
            return None;
        }
    }
    let mut out = String::with_capacity(t.len());
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(' ') => out.push(' '),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            c if c.is_whitespace() => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_unify_and_control_chars_are_dropped() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize("x\x1b[2Jy\u{9b}z\0"), "x[2Jyz");
        assert_eq!(normalize("tab\there"), "tab\there");
        assert_eq!(normalize("日本 🙂"), "日本 🙂");
    }

    #[test]
    fn one_image_path_or_data_url_is_an_image_spec() {
        for (paste, want) in [
            ("shot.png", "shot.png"),
            ("  /ws/a/Shot.JPG \n", "/ws/a/Shot.JPG"),
            ("'my shot.webp'", "my shot.webp"),
            ("\"my shot.gif\"", "my shot.gif"),
            ("my\\ shot.jpeg", "my shot.jpeg"),
            ("data:image/png;base64,AAAA", "data:image/png;base64,AAAA"),
            ("DATA:Image/PNG;base64,AAAA", "DATA:Image/PNG;base64,AAAA"),
        ] {
            assert_eq!(image_spec(paste).as_deref(), Some(want), "{paste:?}");
        }
    }

    #[test]
    fn anything_else_is_text() {
        for paste in [
            "",
            "notes.txt",
            "png",
            "see shot.png",
            "a.png\nb.png",
            "data:text/plain;base64,AAAA",
            "'unterminated.png",
            "'a.png' 'b.png'",
            "fn main() {}",
        ] {
            assert_eq!(image_spec(paste), None, "{paste:?}");
        }
    }
}
