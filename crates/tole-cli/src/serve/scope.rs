//! What a `read-only` device token may do (#1320): `GET` on a route of [`READ`], nothing else.
//! A `full` token and the shared token or Basic pass everything. Every other method, a path
//! that is not listed and a route added later without a line here are refused to a read-only
//! token (fail closed), so no POST needs an exception: there is none.

use tole_cli::serve_support::device_tokens::Scope;

/// The 403 answer for a read-only token.
pub(super) const REFUSED: &str = "this token is read-only";

/// The reads: `{..}` is one non-empty path segment.
const READ: &[&str] = &[
    "/health",
    "/info",
    "/ready",
    "/models",
    "/sessions",
    "/sessions/index",
    "/sessions/{id}",
    "/sessions/{id}/status",
    "/sessions/{id}/entries",
    "/sessions/{id}/events",
    "/sessions/{id}/tree",
    "/sessions/{id}/images/{hash}",
    "/approvals",
    "/missions/{id}",
    "/discussions",
    "/discussions/{id}",
    "/discussions/{id}/events",
    "/profiles",
    "/skills",
    "/tools",
    "/cron/jobs",
    "/cron/runs",
];

pub(super) fn allows(scope: Scope, method: &str, path: &str) -> bool {
    scope == Scope::Full || (method == "GET" && READ.iter().any(|t| matches(t, path)))
}

fn matches(template: &str, path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    let (mut t, mut p) = (template.split('/'), path.split('/'));
    loop {
        match (t.next(), p.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if a.starts_with('{') && !b.is_empty() || a == b => {}
            _ => return false,
        }
    }
}
