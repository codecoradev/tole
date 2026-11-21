use super::*;

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn a_tool_is_named_as_server_dot_tool_and_the_server_must_be_configured() {
    let servers = s(&["nginjen", "uteke"]);
    assert_eq!(
        resolve(&s(&["nginjen.search", "uteke.recall"]), &servers).unwrap(),
        s(&["mcp_nginjen_search", "mcp_uteke_recall"])
    );
    // A tool name that needs no change to be registered resolves as it is.
    assert_eq!(
        resolve(&s(&["nginjen.web-search_2"]), &servers).unwrap(),
        s(&["mcp_nginjen_web-search_2"])
    );
    for bad in ["search", ".search", "nginjen.", "", "nginjen"] {
        let e = resolve(&s(&[bad]), &servers).unwrap_err();
        assert!(e.contains("<server>.<tool>"), "{bad:?}: {e}");
    }
    let e = resolve(&s(&["other.search"]), &servers).unwrap_err();
    assert!(
        e.contains("no connected server `other`") && e.contains("nginjen, uteke"),
        "{e}"
    );
    assert!(resolve(&s(&["a.b"]), &[]).unwrap_err().contains("none"));
    assert!(resolve(&[], &servers).unwrap().is_empty());
}

#[test]
fn the_registration_and_the_allowance_spell_a_tool_alike() {
    assert_eq!(
        discussion::mcp_registry_name("cora", "cora_review_prepare"),
        "mcp_cora_cora_review_prepare"
    );
    let long = discussion::mcp_registry_name("srv", &"x".repeat(100));
    assert_eq!(long.len(), 64);
    assert!(discussion::tool_name_ok(&long));
    // A server name that is not ASCII, cut in the middle of a character,
    // must not panic: the name stays within 64 bytes and valid UTF-8.
    let wide = discussion::mcp_registry_name(&format!("a{}", "é".repeat(31)), "tool");
    assert!(wide.len() <= 64 && wide.starts_with("mcp_"), "{wide}");
}

#[test]
fn the_approvals_queue_must_be_on_for_tools() {
    let cfg = |mode: &str| {
        let src = if mode.is_empty() {
            String::new()
        } else {
            format!("[delegation]\napprovals = \"{mode}\"\n")
        };
        crate::config::parse(&src, std::path::Path::new("u.toml")).unwrap()
    };
    for on in ["owner", "chain"] {
        assert!(need_approvals(&cfg(on)).is_ok(), "{on}");
    }
    for off in ["", "off"] {
        let e = need_approvals(&cfg(off)).unwrap_err();
        assert!(e.contains("approvals = \"owner\""), "{off:?}: {e}");
    }
}

/// The registry name is not one-to-one (characters are replaced, the name is
/// cut), so a spelling that registration would change is refused: the allowance
/// must name the tool the owner typed, never a neighbour with the same
/// registry name.
#[test]
fn a_spelling_that_registration_would_change_is_refused_with_its_registry_name() {
    let servers = s(&["nginjen"]);
    for (spec, registry) in [
        ("nginjen.web.search", "mcp_nginjen_web_search"),
        ("nginjen.web search", "mcp_nginjen_web_search"),
        ("nginjen.search!", "mcp_nginjen_search_"),
    ] {
        let e = resolve(&s(&[spec]), &servers).unwrap_err();
        assert!(
            e.contains(spec) && e.contains(registry) && e.contains("replaced"),
            "{spec}: {e}"
        );
    }
    // Too long: cut at 64 bytes, so it would name whichever tool registers
    // first under the prefix.
    let long = format!("nginjen.{}", "x".repeat(100));
    let e = resolve(&s(&[&long]), &servers).unwrap_err();
    let cut = discussion::mcp_registry_name("nginjen", &"x".repeat(100));
    assert!(e.contains("cut") && e.contains(&cut), "{e}");
    // The longest tool that still fits is accepted whole.
    let max = "x".repeat(64 - "mcp_nginjen_".len());
    let ok = resolve(&s(&[&format!("nginjen.{max}")]), &servers).unwrap();
    assert_eq!(ok, s(&[&format!("mcp_nginjen_{max}")]));
    assert_eq!(ok[0].len(), 64);
    // A non-ASCII tool name is replaced character by character: refused too.
    assert!(resolve(&s(&["nginjen.rech\u{e9}rche"]), &servers).is_err());
}

#[test]
fn a_name_another_configured_server_could_also_produce_is_refused() {
    // Cannot happen with names `McpServerConfig::parse` accepts; the check is
    // a backstop if a configured name ever holds an underscore.
    let servers = s(&["a", "a_b"]);
    let e = resolve(&s(&["a.b_c"]), &servers).unwrap_err();
    assert!(e.contains("mcp_a_b_c") && e.contains("`a_b`"), "{e}");
    assert_eq!(
        resolve(&s(&["a.c"]), &servers).unwrap(),
        s(&["mcp_a_c"]),
        "an unambiguous tool still resolves"
    );
}
