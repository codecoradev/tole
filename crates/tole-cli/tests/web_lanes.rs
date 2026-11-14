//! #625 part 2: which lane each web tool uses, and the `tole doctor` lines
//! (names only, never a URL or a key).

use tole_cli::tool_roster::{web_doctor_lines, web_lanes, WebLane};

const KEY: &str = "nginjen-lanes-key-0123456789";

fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k: &str| {
        pairs
            .iter()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
    }
}

const NGINJEN: &[(&str, &str)] = &[
    ("NGINJEN_BASE_URL", "https://nginjen.internal.example/"),
    ("NGINJEN_API_KEY", KEY),
];

#[test]
fn a_generic_backend_wins_then_nginjen_then_the_built_in() {
    let builtin_search = if cfg!(feature = "web-builtin") {
        WebLane::Builtin
    } else {
        WebLane::Absent
    };
    let builtin_fetch = if cfg!(feature = "web-builtin") {
        WebLane::Builtin
    } else {
        WebLane::Direct
    };
    assert_eq!(
        web_lanes(&env(&[])),
        Ok((builtin_search, builtin_fetch.clone()))
    );
    let ng = WebLane::Nginjen("https://nginjen.internal.example".into(), KEY.into());
    assert_eq!(web_lanes(&env(NGINJEN)), Ok((ng.clone(), ng.clone())));
    // Only one of the two nginjen variables: not the backend.
    assert_eq!(
        web_lanes(&env(&[("NGINJEN_API_KEY", KEY)])).unwrap().1,
        builtin_fetch
    );
    // A generic backend wins, per tool.
    let both: &[(&str, &str)] = &[
        ("NGINJEN_BASE_URL", "https://nginjen.internal.example"),
        ("NGINJEN_API_KEY", KEY),
        ("TOLE_WEB_SEARCH_URL", "https://search.example"),
    ];
    assert_eq!(web_lanes(&env(both)), Ok((WebLane::Generic, ng)));
    assert!(web_lanes(&env(&[
        ("NGINJEN_BASE_URL", "ftp://x"),
        ("NGINJEN_API_KEY", KEY)
    ]))
    .is_err());
}

#[test]
fn doctor_names_the_lanes_and_never_the_url_or_key() {
    let lines = web_doctor_lines(&env(NGINJEN)).join("\n");
    assert!(
        lines.contains("web_search: nginjen backend")
            && lines.contains("web_fetch: nginjen backend"),
        "{lines}"
    );
    assert!(
        !lines.contains(KEY) && !lines.contains("internal.example"),
        "{lines}"
    );
    let lines = web_doctor_lines(&env(&[])).join("\n");
    if cfg!(feature = "web-builtin") {
        assert!(
            lines.contains("web_search: built-in (DuckDuckGo Lite"),
            "{lines}"
        );
        assert!(lines.contains("web_fetch: built-in"), "{lines}");
    } else {
        assert!(
            lines.contains("web_search: off") && lines.contains("web_fetch: direct"),
            "{lines}"
        );
    }
    let bad = web_doctor_lines(&env(&[
        ("NGINJEN_BASE_URL", "ftp://x"),
        ("NGINJEN_API_KEY", KEY),
    ]))
    .join("\n");
    assert!(bad.contains("ERROR") && !bad.contains(KEY), "{bad}");
}

struct Deny;

impl tole_core::approval::Approver for Deny {
    fn decide(&self, _req: &tole_core::approval::ToolRequest<'_>) -> tole_core::approval::Verdict {
        tole_core::approval::Verdict::Deny
    }
}

/// The lanes reach the registry: with both nginjen variables set, the
/// registered `web_fetch` and `web_search` are the nginjen ones. (The only
/// test here that touches the process environment.)
#[test]
fn the_registry_registers_the_lanes_tools() {
    std::env::set_var("NGINJEN_BASE_URL", "https://nginjen.internal.example");
    std::env::set_var("NGINJEN_API_KEY", KEY);
    for k in ["TOLE_WEB_SEARCH_URL", "TOLE_WEB_FETCH_URL"] {
        std::env::remove_var(k);
    }
    let dir = std::env::temp_dir().join(format!("tole-web-lanes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let reg = tole_cli::tool_roster::build(Deny, tole_cli::tool_roster::Roster::new(dir)).unwrap();
    std::env::remove_var("NGINJEN_BASE_URL");
    std::env::remove_var("NGINJEN_API_KEY");
    let specs = reg.specs();
    let desc = |name: &str| {
        specs
            .iter()
            .find(|s| s["function"]["name"] == name)
            .map(|s| {
                s["function"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
    };
    assert!(desc("web_fetch").unwrap().contains("nginjen backend"));
    assert!(desc("web_search").is_some(), "search through the backend");
    assert!(
        !format!("{specs:?}").contains(KEY),
        "the key is never in a spec"
    );
}
