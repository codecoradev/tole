//! Percent-decoding of the ids a request names (a path segment or a query value).
//!
//! `tole-client` percent-encodes every path segment and query value it sends
//! (`weekly report` is `weekly%20report`), so the server decodes the ones it reads
//! as an id or a name. One place does it, strictly:
//!
//! - It runs AFTER routing. `route` still splits the RAW path on `/` and `?` and
//!   matches the raw static parts, so a decoded `/`, `?` or `&` is data of one
//!   value and can never add a segment, start a query or reach another route.
//! - A value with no `%` in it is returned as it came, byte for byte: what needed
//!   no encoding behaves as before.
//! - A `%` must be followed by two hex digits and the decoded bytes must be valid
//!   UTF-8 (no lossy replacement): anything else is a `400`, never a panic.
//! - A value that decodes to a `/`, a NUL or a control character names nothing
//!   (no id holds one), so it is not decoded: the route looks up the raw text,
//!   which no id equals and which answers its usual "no such" (`404`, or the
//!   route's own `400` for an id of the wrong shape), as it did before decoding.
//! - One pass only: `%252F` is the three characters `%2F`, never decoded again.
//! - `+` is a plus, in a path and in a query: the client sends a plus as `%2B`
//!   and a space as `%20`, so a form-style `+` is not read as a space.
//!
//! The decoded value then meets the same checks the route always applied (a
//! session id is `[A-Za-z0-9_-]`, 1..=64; a discussion id its own rule; a job is
//! looked up by exact id), so an encoded form of a valid id decodes to that id and
//! nothing else becomes valid.

use std::borrow::Cow;

use serde_json::{json, Value};

/// Why a value was refused.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    /// A `%` not followed by two hex digits.
    BadEscape,
    /// The decoded bytes are not UTF-8.
    NotUtf8,
    /// The decoded text holds a `/`, a NUL or a control character.
    Forbidden,
}

impl DecodeError {
    fn message(&self) -> &'static str {
        match self {
            DecodeError::BadEscape => "malformed percent-escape in the request target",
            DecodeError::NotUtf8 => "percent-decoded value is not valid UTF-8",
            DecodeError::Forbidden => {
                "percent-decoded value holds a '/', a NUL or a control character"
            }
        }
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode one path segment or query value. See the module doc for the rules.
/// [`DecodeError::Forbidden`] is for the caller to turn into "names nothing".
pub(super) fn decode(raw: &str) -> Result<Cow<'_, str>, DecodeError> {
    if !raw.contains('%') {
        return Ok(Cow::Borrowed(raw));
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).copied().and_then(hex);
            let lo = bytes.get(i + 2).copied().and_then(hex);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(DecodeError::BadEscape);
            };
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let text = String::from_utf8(out).map_err(|_| DecodeError::NotUtf8)?;
    if text.chars().any(|c| c == '/' || c.is_control()) {
        return Err(DecodeError::Forbidden);
    }
    Ok(Cow::Owned(text))
}

/// The `400` answer of a refused value.
fn refusal(e: &DecodeError) -> (u16, Value) {
    (400, json!({"error": e.message()}))
}

/// [`decode`] for a route: the id, or the `400` to answer. A value that decodes
/// to a `/`, a NUL or a control character is returned as its raw text (see the
/// module doc), never as the decoded one.
pub(super) fn id(raw: &str) -> Result<Cow<'_, str>, (u16, Value)> {
    match decode(raw) {
        Err(DecodeError::Forbidden) => Ok(Cow::Borrowed(raw)),
        other => other.map_err(|e| refusal(&e)),
    }
}

/// The raw value of the first `key=` of a query string (`a=1&b=2`).
pub(super) fn raw_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
}

/// The decoded value of the first `key=` of a query string: `Ok(None)` when it
/// is absent, the `400` to answer when its value is refused.
pub(super) fn param<'a>(query: &'a str, key: &str) -> Result<Option<Cow<'a, str>>, (u16, Value)> {
    raw_param(query, key).map(id).transpose()
}

/// The id of a route shaped `{prefix}{id}{suffix}`, or `None` when the path is
/// not of that shape. The id may be empty (`/discussions//resume`) and the
/// caller refuses it. A path that is only the suffix after the collection
/// (`/discussions/resume`) starts with the prefix and ends with the suffix at
/// once, and slicing it by the two lengths made a reversed range and a panic,
/// so the prefix is stripped first and the suffix from what is left.
pub(super) fn id_between<'a>(path: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)?.strip_suffix(suffix)
}

/// Runs `f` with the percent-decoded id of a route matched by [`id_between`]
/// (the match is on the raw path, the id is decoded after it: see `pct`), or
/// answers the `400` of a value that does not decode.
pub(super) fn with_id(
    raw: Option<&str>,
    f: impl FnOnce(&str) -> (u16, serde_json::Value),
) -> (u16, serde_json::Value) {
    match id(raw.unwrap_or_default()) {
        Ok(id) => f(&id),
        Err(refused) => refused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> String {
        decode(raw).unwrap().into_owned()
    }

    #[test]
    fn a_value_without_a_percent_is_returned_untouched() {
        for raw in [
            "",
            "srv-1_a.b~",
            "a+b",
            "a b",
            "a/b",
            "a\u{0}b",
            "wëekly",
            "..",
        ] {
            assert!(
                matches!(decode(raw), Ok(Cow::Borrowed(s)) if s == raw),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_valid_escape_decodes_in_either_case() {
        assert_eq!(ok("weekly%20report"), "weekly report");
        assert_eq!(ok("%73rv-1"), "srv-1");
        assert_eq!(ok("%7e%7E"), "~~");
        assert_eq!(ok("a%2Bb"), "a+b");
        // Multi-byte UTF-8, upper and lower case hex.
        assert_eq!(ok("%E2%9C%93%e2%9c%93"), "\u{2713}\u{2713}");
        assert_eq!(ok("%D0%BE%D1%82%D1%87%D0%B5%D1%82"), "отчет");
        // The characters that structure a target are data once decoded.
        assert_eq!(ok("a%3Fb%26c%3Dd%23e"), "a?b&c=d#e");
    }

    #[test]
    fn a_plus_is_a_plus() {
        assert_eq!(ok("a+b%20c"), "a+b c");
        assert_eq!(ok("%2B"), "+");
        assert_eq!(ok("++"), "++");
    }

    #[test]
    fn a_malformed_escape_is_refused() {
        for raw in [
            "%", "a%", "%2", "a%2", "%zz", "%2z", "%z2", "%%20", "%-1", "%2%20",
        ] {
            assert_eq!(decode(raw), Err(DecodeError::BadEscape), "{raw:?}");
        }
    }

    #[test]
    fn invalid_utf8_is_refused_never_replaced() {
        for raw in [
            "%ff",
            "%C3",          // truncated two-byte sequence
            "%E2%9C",       // truncated three-byte sequence
            "%80",          // a lone continuation byte
            "%C0%AF",       // overlong `/`
            "%C0%80",       // overlong NUL
            "%E0%80%AF",    // overlong three-byte `/`
            "%F0%80%80%AF", // overlong four-byte `/`
            "%ED%A0%80",    // a UTF-16 surrogate
            "%F4%90%80%80", // above U+10FFFF
            "ok%FEok",
        ] {
            assert_eq!(decode(raw), Err(DecodeError::NotUtf8), "{raw:?}");
        }
    }

    #[test]
    fn a_decoded_slash_nul_or_control_is_refused() {
        for raw in [
            "%2F",
            "%2f",
            "a%2Fb",
            "%2e%2e%2f",
            "..%2F..%2Fetc",
            "%00",
            "a%00b",
            "%0A",
            "%0d%0a",
            "%09",
            "%7F",
            "%C2%80", // a C1 control character
            "%C2%85",
        ] {
            assert_eq!(decode(raw), Err(DecodeError::Forbidden), "{raw:?}");
        }
        // The dots alone are an ordinary value; the route's own check refuses them.
        assert_eq!(ok("%2e%2e"), "..");
    }

    #[test]
    fn it_decodes_once() {
        assert_eq!(ok("%252F"), "%2F");
        assert_eq!(ok("%2525"), "%25");
        assert_eq!(ok("%25"), "%");
        assert_eq!(ok("%2500"), "%00");
    }

    #[test]
    fn a_huge_value_is_linear_and_bounded_by_its_input() {
        let raw = "%41".repeat(400_000);
        assert_eq!(ok(&raw).len(), 400_000);
        let bad = format!("{raw}%");
        assert_eq!(decode(&bad), Err(DecodeError::BadEscape));
        let plain = "a".repeat(1 << 20);
        assert!(matches!(decode(&plain), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn the_encoder_of_the_client_round_trips() {
        // The client leaves `[A-Za-z0-9-_.~]` and writes every other byte as `%XX`.
        let enc = |s: &str| -> String {
            s.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect()
        };
        for s in ["weekly report", "ünï-çode ✓", "a+b&c=d?e#f", "100%"] {
            assert_eq!(
                decode(&enc(s)).map(Cow::into_owned),
                Ok(s.to_string()),
                "{s}"
            );
        }
        // Only the slash and the controls are refused, by design.
        assert_eq!(decode(&enc("a/b")), Err(DecodeError::Forbidden));
        assert_eq!(decode(&enc("a\nb")), Err(DecodeError::Forbidden));
    }

    #[test]
    fn a_query_value_is_read_raw_then_decoded_alone() {
        assert_eq!(
            raw_param("job=a%26limit%3D5&limit=2", "job"),
            Some("a%26limit%3D5")
        );
        // The decoded `&` and `=` stay inside the value; `limit` is the real one.
        assert_eq!(
            param("job=a%26limit%3D5&limit=2", "job")
                .unwrap()
                .as_deref(),
            Some("a&limit=5")
        );
        assert_eq!(
            param("job=a&limit=2", "limit").unwrap().as_deref(),
            Some("2")
        );
        assert_eq!(param("limit=2", "job").unwrap(), None);
        assert_eq!(param("", "job").unwrap(), None);
        assert_eq!(param("job=%zz", "job").unwrap_err().0, 400);
        assert_eq!(param("session=%ff", "session").unwrap_err().0, 400);
        // A key that merely starts with the name is not the name.
        assert_eq!(param("jobs=x", "job").unwrap(), None);
    }

    #[test]
    fn a_slash_nul_or_control_names_nothing_and_stays_raw() {
        for raw in ["%2e%2e%2f", "a%2Fb", "%00", "a%0Ab", "%7F", "%C2%85"] {
            assert_eq!(id(raw).unwrap(), raw, "{raw}");
        }
        assert_eq!(id("%20").unwrap(), " ");
        assert_eq!(
            param("job=a%2Fb&limit=1", "job").unwrap().as_deref(),
            Some("a%2Fb")
        );
    }

    #[test]
    fn a_refusal_is_a_400_with_a_message() {
        let (status, body) = id("%zz").unwrap_err();
        assert_eq!(
            (
                status,
                body["error"].as_str().unwrap().contains("malformed")
            ),
            (400, true)
        );
    }
}
