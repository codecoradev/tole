//! The request path of a route: ids and hashes percent-encoded into its template.

use crate::route::Route;

/// Percent-encode one path segment or query value.
pub(crate) fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The request path of `route`: `{id}` and `{hash}` filled in (each
/// percent-encoded, so a value can never add a segment or a query), then the
/// query string.
pub(crate) fn path_of(route: Route, id: Option<&str>, hash: Option<&str>, query: &str) -> String {
    let mut path = route
        .template()
        .replace("{id}", &pct(id.unwrap_or("")))
        .replace("{hash}", &pct(hash.unwrap_or("")));
    if !query.is_empty() {
        path.push('?');
        path.push_str(query);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_percent_encoded() {
        assert_eq!(pct("srv-1_a.b~"), "srv-1_a.b~");
        assert_eq!(pct("../x y"), "..%2Fx%20y");
        assert_eq!(pct("a\r\nb"), "a%0D%0Ab");
    }
}
