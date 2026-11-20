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
    // A tool name with characters a provider rejects gets the registration's
    // own spelling, so what is allowed is what the helper's registry holds.
    assert_eq!(
        resolve(&s(&["nginjen.web.search"]), &servers).unwrap(),
        s(&["mcp_nginjen_web_search"])
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
