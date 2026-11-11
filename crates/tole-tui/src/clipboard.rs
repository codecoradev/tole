//! `/copy` (#463): the terminal clipboard through OSC 52, with no native
//! clipboard dependency (the TUI has none, decision of #560).
//!
//! OSC 52 ("Manipulate Selection Data") is `ESC ] 52 ; Pc ; Pd ST`, where
//! `Pc` names the selection (`c` = clipboard) and `Pd` is the data in
//! base64; xterm accepts BEL as well as `ESC \` as the terminator.
//!
//! Sources, read on 2026-11-11:
//! - Ghostty, "Query or change data on clipboards (OSC 52)": the sequence is
//!   `ESC ] 52 ; t ; d ESC \`; `t` lists clipboard types "as defined by
//!   xterm" (`c` standard clipboard, `p` primary, `q` secondary, `s`
//!   selection, `0`-`7` cut buffers); `d` is base64 for the new data, and a
//!   `d` that is neither valid base64 nor `?` CLEARS the listed clipboards
//!   (so this module never sends an empty or invalid payload).
//! - foot, `foot-ctlseqs(7)`: `ESC ] 52 ; Pc ; Pd ESC \` "Copy Pd (base64
//!   encoded text) to the clipboard", `c` targets the clipboard.
//! - xterm's own ctlseqs page (invisible-island.net) was NOT read directly:
//!   that host is outside the session's network allowlist. The xterm side
//!   (`Ps = 5 2`, `Pt` parsed as `Pc ; Pd`, BEL and ST both accepted as the
//!   OSC terminator) rests on search excerpts of that document and on the
//!   charmbracelet `x/ansi` docs, which list both `ST` and `BEL` forms.
//!   BEL is kept as the terminator for the widest compatibility; the e2e
//!   test pins the exact bytes.
//!
//! Safety: the payload is ONLY base64 of the answer (alphabet `A-Za-z0-9+/=`),
//! so answer text can never carry an ESC, BEL or `]52;` into the stream.
//! [`osc52`] is the single place that builds the sequence and [`emit`] the
//! single place that writes it; provider text still reaches the screen only
//! through the sanitizer.
//!
//! A terminal gives no reply to OSC 52, so the caller must never claim the
//! copy worked.

use std::io::{self, Write};

/// Largest answer `/copy` sends (raw bytes). Terminals and multiplexers cap
/// the OSC 52 payload (xterm's default is about 100 KB of base64, tmux and
/// others differ), and a silently truncated clipboard is worse than a
/// refusal: above this the UI points to `/export`.
pub const MAX_COPY_BYTES: usize = 64 * 1024;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648 section 4).
pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            n |= (*b as u32) << (16 - 8 * i);
        }
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 write sequence for `text` (clipboard selection, BEL-terminated).
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Write a sequence built by [`osc52`] and flush.
pub fn emit(w: &mut impl Write, seq: &str) -> io::Result<()> {
    w.write_all(seq.as_bytes())?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (raw, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(raw.as_bytes()), want, "{raw:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn the_sequence_is_esc_osc_52_c_base64_bel() {
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x07");
        // Multi-byte text is encoded as its UTF-8 bytes.
        assert_eq!(osc52("é"), "\x1b]52;c;w6k=\x07");
    }

    #[test]
    fn answer_text_cannot_break_out_of_the_payload() {
        let hostile = "a\x1b[2J\x07b\x1b]52;c;ZXZpbA==\x07 \x1b\\ ]52; \r\n\u{9b}";
        let seq = osc52(hostile);
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .and_then(|s| s.strip_suffix('\x07'))
            .expect("fixed prefix and a single BEL terminator");
        assert!(
            payload
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
            "{payload}"
        );
        assert_eq!(seq.matches('\x1b').count(), 1);
        assert_eq!(seq.matches('\x07').count(), 1);
        assert!(!seq.contains("]52;c;ZXZpbA"), "raw text must not appear");
    }

    #[test]
    fn emit_writes_exactly_the_sequence() {
        let mut sink = Vec::new();
        emit(&mut sink, &osc52("x")).unwrap();
        assert_eq!(sink, b"\x1b]52;c;eA==\x07");
    }
}
