//! #1069: `tole-client` and `tole serve` cover the same routes. A route that
//! serve registers and the client has no method for, or a client method for a
//! route serve does not have, fails here, unless the route is on the list of
//! routes the client leaves out on purpose.
//!
//! Three checks keep the list of serve's routes honest, because serve's
//! dispatch is a `match` and cannot be asked for its routes:
//!
//! 1. each listed route is answered by a REAL serve (not the 404 `not found` of
//!    an unknown route), so a route that was renamed or removed is caught;
//! 2. the routing code in `src/serve.rs` is read, and every route shape it
//!    registers must be listed (a new route in the code fails here until it is
//!    listed, and then until the client has a method for it or the allowlist
//!    says why not);
//! 3. the list is compared with `Route::ALL`, both ways.
//!
//! A fourth check (#1024) compares the list with the routes
//! `docs/mobile-control.md` documents, both ways, so the document cannot lose
//! or gain a route unnoticed.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use std::collections::BTreeSet;
use tole_client::Route;

mod common;
mod serve_harness;
use serve_harness::{spawn_mock, Serve};

/// Every route `tole serve` registers: `(method, template)`. `{id}` is a
/// session, approval, discussion, cron job or mission id, `{hash}` an image's
/// hash, `{name}` a webhook route name.
const SERVE_ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/models"),
    ("GET", "/sessions"),
    ("GET", "/sessions/index"),
    ("POST", "/sessions"),
    ("GET", "/sessions/{id}"),
    ("GET", "/sessions/{id}/status"),
    ("GET", "/sessions/{id}/entries"),
    ("GET", "/sessions/{id}/events"),
    ("POST", "/sessions/{id}/prompt"),
    ("POST", "/sessions/{id}/cancel"),
    ("POST", "/sessions/{id}/interrupt"),
    ("POST", "/sessions/{id}/detach"),
    ("POST", "/sessions/{id}/fork"),
    ("GET", "/sessions/{id}/tree"),
    ("POST", "/sessions/{id}/compact"),
    ("POST", "/sessions/{id}/name"),
    ("POST", "/sessions/{id}/model"),
    ("POST", "/sessions/{id}/effort"),
    ("POST", "/sessions/{id}/images"),
    ("GET", "/sessions/{id}/images/{hash}"),
    ("GET", "/approvals"),
    ("POST", "/approvals/{id}/decision"),
    ("POST", "/missions"),
    ("GET", "/missions/{id}"),
    ("POST", "/missions/{id}/cancel"),
    ("GET", "/discussions"),
    ("GET", "/discussions/{id}"),
    ("POST", "/discussions/estimate"),
    ("POST", "/discussions"),
    ("POST", "/discussions/{id}/resume"),
    ("POST", "/discussions/{id}/stop"),
    ("GET", "/profiles"),
    ("GET", "/skills"),
    ("GET", "/tools"),
    ("GET", "/cron/jobs"),
    ("POST", "/cron/jobs/{id}/pause"),
    ("POST", "/cron/jobs/{id}/resume"),
    ("GET", "/cron/runs"),
    ("POST", "/hooks/{name}"),
];

/// Routes of serve the client has no method for, on purpose, each with why.
const NOT_IN_CLIENT: &[(&str, &str, &str)] = &[
    (
        "GET",
        "/sessions/{id}",
        "an alias of GET /sessions/{id}/status, which has a method",
    ),
    (
        "POST",
        "/hooks/{name}",
        "the inbound webhook: an outside sender calls it, authenticated by its own signature, \
         never a UI client with the serve token",
    ),
];

fn pair(m: &str, p: &str) -> (String, String) {
    (m.to_string(), p.to_string())
}

fn served() -> BTreeSet<(String, String)> {
    SERVE_ROUTES.iter().map(|(m, p)| pair(m, p)).collect()
}

fn in_client() -> BTreeSet<(String, String)> {
    Route::ALL
        .iter()
        .map(|r| pair(r.method(), r.template()))
        .collect()
}

/// Check 3: the client and serve cover the same routes, but for the allowlist.
#[test]
fn client_and_serve_cover_the_same_routes() {
    let served = served();
    assert_eq!(served.len(), SERVE_ROUTES.len(), "a route is listed twice");
    let left_out: BTreeSet<_> = NOT_IN_CLIENT.iter().map(|(m, p, _)| pair(m, p)).collect();
    for (m, p, why) in NOT_IN_CLIENT {
        assert!(!why.is_empty());
        assert!(
            served.contains(&pair(m, p)),
            "`{m} {p}` is on the allowlist but is not a serve route: remove it"
        );
    }
    let client = in_client();
    assert_eq!(
        client.len(),
        Route::ALL.len(),
        "a client route is listed twice"
    );

    let no_method: Vec<_> = served
        .difference(&client)
        .filter(|r| !left_out.contains(*r))
        .collect();
    assert!(
        no_method.is_empty(),
        "serve registers routes the client has no method for (add a `Route` and a method, or \
         list the route in NOT_IN_CLIENT with the reason): {no_method:?}"
    );
    let no_route: Vec<_> = client.difference(&served).collect();
    assert!(
        no_route.is_empty(),
        "the client has methods for routes that are not in SERVE_ROUTES: {no_route:?}"
    );
    let stale: Vec<_> = left_out.intersection(&client).collect();
    assert!(
        stale.is_empty(),
        "on the allowlist, yet the client has a method for it: remove the entry: {stale:?}"
    );
}

/// Check 1: a real serve answers each listed route. The only answer an unknown
/// route gets is `404 {"error": "not found"}`. The probes use ids that exist
/// nowhere and an empty body, so none of them changes anything but
/// `POST /sessions`, which opens one session.
#[test]
fn every_listed_serve_route_is_served() {
    let serve = Serve::spawn(&spawn_mock(0));
    for (method, template) in SERVE_ROUTES {
        let path = template
            .replace("{id}", "nope")
            .replace("{hash}", "0123456789abcdef")
            .replace("{name}", "nope");
        let body = (*method == "POST").then_some("{}");
        let (code, v) = serve.http(method, &path, body);
        assert!(
            !(code == 404 && v["error"] == "not found"),
            "serve has no `{method} {template}` (answered {code} {v})"
        );
    }
}

/// The routing code of `src/serve.rs` before its unit tests, without comments.
fn routing_code() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/serve.rs");
    let src = std::fs::read_to_string(&path).unwrap();
    let end = src
        .find("#[cfg(test)]\nmod tests {")
        .expect("the unit tests of serve.rs start with `#[cfg(test)] mod tests`");
    src[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The string literals of `text`, in order (`\"` does not end one).
fn literals(text: &str) -> Vec<String> {
    text.replace("\\\"", "'")
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// Check 2: every route shape the routing code registers is listed. Four shapes
/// cover it today; a route written another way would not be seen here, so a
/// new shape needs a new arm in this function.
#[test]
fn every_route_the_routing_code_registers_is_listed() {
    let code = routing_code();
    let served = served();
    let templates: BTreeSet<&str> = SERVE_ROUTES.iter().map(|(_, p)| *p).collect();
    let mut found: BTreeSet<(String, String, &str)> = BTreeSet::new();

    for line in code.lines().map(str::trim) {
        // `("GET", "/health") =>`: a method and an exact path.
        for method in ["GET", "POST"] {
            if let Some(rest) = line.strip_prefix(&format!("(\"{method}\", \"")) {
                if let Some(path) = rest.split('"').next().filter(|p| p.starts_with('/')) {
                    found.insert((method.into(), path.into(), "an exact path"));
                }
            }
            // `("POST", id, Some("fork")) =>`: an action of a session.
            if let Some(rest) = line.strip_prefix(&format!("(\"{method}\", id,")) {
                if rest.contains("None") {
                    found.insert((method.into(), "/sessions/{id}".into(), "a bare id"));
                }
                for action in literals(rest) {
                    found.insert((
                        method.into(),
                        format!("/sessions/{{id}}/{action}"),
                        "a session action",
                    ));
                }
            }
        }
        // `id_between(p, "/missions/", "/cancel")`: a POST on `{prefix}{id}{suffix}`.
        if let Some(rest) = line.split("id_between(p, ").nth(1) {
            if let [prefix, suffix, ..] = literals(rest).as_slice() {
                found.insert((
                    "POST".into(),
                    format!("{prefix}{{id}}{suffix}"),
                    "an id between",
                ));
            }
        }
    }
    for (method, path, shape) in &found {
        assert!(
            served.contains(&pair(method, path)),
            "serve.rs registers `{method} {path}` ({shape}) and SERVE_ROUTES does not list it: \
             add it there, and a client method or a NOT_IN_CLIENT entry"
        );
    }
    // The shapes were really found: a scan that sees nothing proves nothing.
    assert!(found.len() >= 25, "the scan found only {}", found.len());

    // Whatever else the code spells as a path (`starts_with("/missions/")`, the
    // events and images helpers) must at least use words the list knows, so a
    // route under a new name cannot slip by.
    let known: BTreeSet<&str> = templates
        .iter()
        .flat_map(|t| t.split('/'))
        .filter(|s| !s.is_empty() && !s.starts_with('{'))
        .collect();
    for lit in literals(&code).iter().filter(|l| l.starts_with('/')) {
        for word in lit.split('?').next().unwrap().split('/') {
            assert!(
                word.is_empty() || known.contains(word),
                "serve.rs spells the path `{lit}` and `{word}` is in no listed route: list the \
                 new route in SERVE_ROUTES"
            );
        }
    }
}

/// The scan above and the list fail in the right direction: a list without a
/// route the code has is the failure the test exists for.
#[test]
fn the_scan_is_not_vacuous() {
    let code = routing_code();
    for needle in [
        "(\"GET\", \"/health\")",
        "(\"POST\", id, Some(\"fork\"))",
        "id_between(p, \"/missions/\", \"/cancel\")",
    ] {
        assert!(code.contains(needle), "the scan no longer sees `{needle}`");
    }
    assert_eq!(literals("a \"x\" b \"y\\\"z\" c").len(), 2);
}

/// Serve routes the document does not name as `METHOD /path`, with why.
const NOT_DOCUMENTED: &[(&str, &str, &str)] = &[(
    "GET",
    "/sessions/{id}",
    "an alias of GET /sessions/{id}/status, which the document names",
)];

/// A documented path as a route template: any `{name}`, `<name>` or `dap-…`
/// segment is an id, but the image hash and the webhook name keep their words.
fn template_of(path: &str) -> String {
    let segs: Vec<&str> = path.split('/').collect();
    let id_like = |s: &str| s.starts_with('{') || s.starts_with('<') || s.contains('…');
    segs.iter()
        .enumerate()
        .map(|(i, s)| match (i, *s) {
            (_, s) if !id_like(s) => s.to_string(),
            (4, _) if segs[3] == "images" => "{hash}".to_string(),
            (2, _) if segs[1] == "hooks" => "{name}".to_string(),
            _ => "{id}".to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The `` `METHOD /path` `` routes the document names, as `(method, template)`.
fn documented() -> BTreeSet<(String, String)> {
    let doc = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/mobile-control.md");
    let text = std::fs::read_to_string(&doc).unwrap();
    let mut out = BTreeSet::new();
    for part in text.split('`') {
        let Some((method, rest)) = part.split_once(' ') else {
            continue;
        };
        if !matches!(method, "GET" | "POST") || !rest.starts_with('/') {
            continue;
        }
        let path = rest.split(['?', ' ']).next().unwrap();
        out.insert(pair(method, &template_of(path)));
    }
    out
}

/// Check 4: the document and serve name the same routes.
#[test]
fn the_documented_routes_are_the_served_routes() {
    let doc = documented();
    assert!(
        doc.len() >= 35,
        "the scan of the document found {}",
        doc.len()
    );
    let served = served();
    let skip: BTreeSet<_> = NOT_DOCUMENTED.iter().map(|(m, p, _)| pair(m, p)).collect();
    for (m, p, why) in NOT_DOCUMENTED {
        assert!(
            !why.is_empty() && served.contains(&pair(m, p)),
            "stale entry `{m} {p}`"
        );
        assert!(
            !doc.contains(&pair(m, p)),
            "`{m} {p}` is documented now: drop it from NOT_DOCUMENTED"
        );
    }
    let missing: Vec<_> = served
        .difference(&doc)
        .filter(|r| !skip.contains(*r))
        .collect();
    assert!(
        missing.is_empty(),
        "serve answers routes docs/mobile-control.md does not name (document them, as `METHOD /path`): {missing:?}"
    );
    let gone: Vec<_> = doc.difference(&served).collect();
    assert!(
        gone.is_empty(),
        "docs/mobile-control.md names routes serve does not have (removing a route is a protocol \
         break; see \"Wire compatibility\"): {gone:?}"
    );
}
