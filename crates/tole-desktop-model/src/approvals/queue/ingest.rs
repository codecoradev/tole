//! A request on the way in: checked and cut to size, so the queue stores
//! bounded data whatever the server sends (the card bounds it again to draw).

use super::super::request::ApprovalRequest;
use super::super::summary::{bound, ActionSummary, MAX_DETAIL_CHARS, MAX_DISPLAY_CHARS};

/// Longest approval id accepted.
pub const MAX_ID_LEN: usize = 128;
/// Longest session id accepted (the core's own rule).
const MAX_SESSION_ID_LEN: usize = 64;
/// Longest fingerprint accepted; it is sent back as it came, never cut.
const MAX_FINGERPRINT_LEN: usize = 256;
const MAX_CODE_CHARS: usize = 200;
const MAX_STATUS_CHARS: usize = 64;
const MAX_PREVIEW_LINES: usize = 60;
const MAX_NOTIFIED: usize = 8;
/// Room for the cut marker inside a bound.
const MARKER_ROOM: usize = 100;

/// An id or session id the model keeps and the adapter puts in a URL:
/// `[A-Za-z0-9_-]`, 1 to `max` bytes. Safe to draw as it is.
pub fn valid_key(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// `request`, bounded; `None` for a row that cannot be kept: an id or session
/// id outside the charset (an empty session id is the absence of one) or a
/// fingerprint too long to be one.
pub fn admit(mut r: ApprovalRequest) -> Option<ApprovalRequest> {
    if !valid_key(&r.id, MAX_ID_LEN) {
        return None;
    }
    match r.session_id.as_deref() {
        Some("") => r.session_id = None,
        Some(s) if !valid_key(s, MAX_SESSION_ID_LEN) => return None,
        _ => {}
    }
    if r.fingerprint
        .as_ref()
        .is_some_and(|f| f.len() > MAX_FINGERPRINT_LEN)
    {
        return None;
    }
    r.status = head(r.status, MAX_STATUS_CHARS);
    r.tool = shrink(r.tool, MAX_DISPLAY_CHARS).0;
    r.description = r.description.map(|d| shrink(d, MAX_DETAIL_CHARS).0);
    r.risk = r.risk.map(|v| head(v, MAX_STATUS_CHARS));
    r.child_node = r.child_node.map(|n| shrink(n, MAX_DISPLAY_CHARS).0);
    r.notified.truncate(MAX_NOTIFIED);
    for n in &mut r.notified {
        *n = shrink(std::mem::take(n), MAX_DISPLAY_CHARS).0;
    }
    r.summary = r.summary.map(admit_summary);
    Some(r)
}

fn admit_summary(mut s: ActionSummary) -> ActionSummary {
    s.kind = head(s.kind, MAX_CODE_CHARS);
    s.effect = head(s.effect, MAX_CODE_CHARS);
    let (target, cut) = shrink(s.target, MAX_DISPLAY_CHARS);
    s.target = target;
    s.target_truncated |= cut;
    let mut more = s.preview.len() > MAX_PREVIEW_LINES;
    s.preview.truncate(MAX_PREVIEW_LINES);
    for line in &mut s.preview {
        let (l, cut) = shrink(std::mem::take(line), MAX_DISPLAY_CHARS);
        *line = l;
        more |= cut;
    }
    s.preview_truncated |= more;
    s
}

/// The first `max` characters.
fn head(s: String, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_owned(),
        None => s,
    }
}

/// `s` unchanged when it fits, else its head and its tail around a marker
/// (a dangerous tail is never dropped) in at most `max` characters; the flag
/// says it was cut.
fn shrink(s: String, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s, false);
    }
    (bound(&s, max - MARKER_ROOM).0, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long() -> String {
        "x".repeat(30_000)
    }

    #[test]
    fn a_request_is_cut_to_size_on_the_way_in() {
        let mut r = ApprovalRequest::new("a", &long(), "pending");
        r.description = Some(long());
        r.notified = vec![long(); 20];
        r.child_node = Some(long());
        let mut s = ActionSummary::unknown("t", None);
        s.target = long();
        s.preview = vec![long(); 100];
        r.summary = Some(s);
        let r = admit(r).unwrap();
        let s = r.summary.unwrap();
        let chars = |t: &str| t.chars().count();
        assert!(chars(&r.tool) <= MAX_DISPLAY_CHARS && chars(&s.target) <= MAX_DISPLAY_CHARS);
        assert!(chars(&r.description.unwrap()) <= MAX_DETAIL_CHARS);
        assert!(r.notified.len() <= MAX_NOTIFIED && s.preview.len() <= MAX_PREVIEW_LINES);
        assert!(s.preview.iter().all(|l| chars(l) <= MAX_DISPLAY_CHARS));
        assert!(s.target_truncated && s.preview_truncated);
        assert!(chars(&r.child_node.unwrap()) <= MAX_DISPLAY_CHARS);
    }

    #[test]
    fn a_cut_keeps_the_tail_and_a_short_text_is_untouched() {
        let text = format!("{}TAIL_EVIL", "x".repeat(50_000));
        let (cut, was) = shrink(text, MAX_DETAIL_CHARS);
        assert!(was && cut.ends_with("TAIL_EVIL"));
        assert_eq!(shrink("short".into(), 200), ("short".to_owned(), false));
        assert_eq!(head("héllo".into(), 2), "hé");
    }

    #[test]
    fn ids_are_keys_or_the_row_is_refused() {
        let ok = |id: &str, s: Option<&str>| {
            let mut r = ApprovalRequest::new(id, "t", "pending");
            r.session_id = s.map(str::to_owned);
            admit(r)
        };
        assert!(ok("apr-1_A", Some("ses-1")).is_some());
        assert_eq!(ok("a", Some("")).unwrap().session_id, None);
        for bad in ["", "a b", "a/b", "a\u{202e}b", "é", &"a".repeat(129)] {
            assert!(ok(bad, None).is_none(), "{bad:?}");
            assert!(ok("a", Some(bad)).is_none() || bad.is_empty(), "{bad:?}");
        }
        let mut r = ApprovalRequest::new("a", "t", "pending");
        r.fingerprint = Some("f".repeat(257));
        assert!(admit(r).is_none());
    }
}
