use super::*;

const OK: &str = r#"
name = "acme"
version = "1.2.0"
description = "Acme notes"
paths = ["~/notes"]

[mcp_servers.acme-notes]
run = "acme-mcp --stdio"
secrets = ["ACME_TOKEN"]

[[skills]]
name = "triage"
path = "skills/triage/SKILL.md"

[[hooks]]
event = "pretool"
command = "./hooks/check.sh"
"#;

#[test]
fn a_manifest_parses_with_servers_secrets_skills_hooks_and_paths() {
    let b = Bundle::parse(OK).unwrap();
    assert_eq!((b.name.as_str(), b.version.as_str()), ("acme", "1.2.0"));
    let s = &b.mcp_servers["acme-notes"];
    assert_eq!(s.spec("acme-notes"), "acme-notes=acme-mcp --stdio");
    assert_eq!(s.secrets, ["ACME_TOKEN"]);
    assert_eq!((b.skills.len(), b.hooks.len(), b.paths.len()), (1, 1, 1));
}

#[test]
fn a_manifest_that_could_widen_a_grant_or_inject_is_refused() {
    let with = |extra: &str| format!("name = \"a\"\nversion = \"1\"\n{extra}");
    let server = |body: &str| with(&format!("[mcp_servers.s]\n{body}\n"));
    let bad = [
        // Unknown keys are errors, not ignored.
        with("grant_all = true\n[mcp_servers.s]\nrun = \"x\"\n"),
        // No server at all.
        with(""),
        // run and url together, or neither.
        server("run = \"x\"\nurl = \"https://e.example\""),
        server("secrets = []"),
        // A remote server cannot hold a grant.
        server("url = \"https://e.example\"\nsecrets = [\"A_TOKEN\"]"),
        // Not secret-shaped, lower case, odd characters, duplicate.
        server("run = \"x\"\nsecrets = [\"HOME\"]"),
        server("run = \"x\"\nsecrets = [\"acme_token\"]"),
        server("run = \"x\"\nsecrets = [\"A_TOKEN=1\"]"),
        server("run = \"x\"\nsecrets = [\"A_TOKEN\", \"A_TOKEN\"]"),
        // Control characters, bad names.
        server("run = \"x\\ny\""),
        "name = \"a b\"\nversion = \"1\"\n[mcp_servers.s]\nrun = \"x\"\n".to_string(),
        "name = \"a\"\nversion = \"1\"\n[mcp_servers.\"s/../x\"]\nrun = \"x\"\n".to_string(),
        with("description = \"\\u001b[2J\"\n[mcp_servers.s]\nrun = \"x\"\n"),
    ];
    for text in &bad {
        assert!(Bundle::parse(text).is_err(), "accepted: {text}");
    }
}

#[test]
fn load_hashes_the_exact_bytes_and_any_change_moves_the_hash() {
    let dir = crate::test_dir::TestDir::new("tole-bundle-load");
    let file = dir.join(FILE);
    std::fs::write(&file, OK).unwrap();
    let a = load(&dir).unwrap();
    assert_eq!(a.hash, load(&file).unwrap().hash, "a directory or the file");
    assert_eq!(a.hash, file_hash(&a.path.display().to_string()));
    // A comment changes nothing semantically and still needs a new approval.
    std::fs::write(&file, format!("{OK}\n# harmless\n")).unwrap();
    assert_ne!(load(&dir).unwrap().hash, a.hash);
    assert_eq!(file_hash("/nonexistent/tole-plugin.toml"), "");
}

#[test]
fn a_huge_or_non_text_manifest_is_refused() {
    let dir = crate::test_dir::TestDir::new("tole-bundle-big");
    let file = dir.join(FILE);
    std::fs::write(&file, vec![b'#'; MAX_BYTES as usize + 1]).unwrap();
    assert!(load(&file).unwrap_err().contains("larger"));
    std::fs::write(&file, [0xff, 0xfe]).unwrap();
    assert!(load(&file).unwrap_err().contains("UTF-8"));
    assert!(load(&dir.join("missing.toml")).is_err());
}
