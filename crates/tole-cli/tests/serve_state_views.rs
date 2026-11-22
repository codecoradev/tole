//! #1042 the owner's profiles, skills and connected tools over the REAL `tole
//! serve` face, through the typed client. They are read-only: no verb on these
//! paths trusts a skill or a tool, changes the allow list or edits a profile,
//! and reading them starts no connected server.

#![cfg(feature = "shell-tools")]

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use tole_client::Client;
use tole_core::skills::curator::Archive;
use tole_core::skills::draft::{Draft, DraftStore};

mod common;
mod serve_harness;
use serve_harness::{home_of, spawn_mock, workspace_dir, Serve};

const CONFIG: &str = r#"
[profiles.reviewer]
system_prompt_file = "reviewer.md"
allow = ["web_search"]
memory_room = "notes"

[profiles.reviewer.models]
standard = "m-std"
advisor = "m-adv"

[profiles.lost]
system_prompt_file = "missing.md"
"#;

fn skill(dir: &Path, name: &str, body: &str) {
    let d = dir.join("skills").join(name);
    std::fs::create_dir_all(&d).unwrap();
    let md = format!("---\nname: {name}\ndescription: \"{name} skill\"\n---\n{body}");
    std::fs::write(d.join("SKILL.md"), md).unwrap();
}

/// A serve over a home with two profiles (one broken), a skill, a skill draft,
/// an archived skill and one configured connected server that would leave a
/// marker file if anything started it.
fn serve() -> (Serve, std::path::PathBuf) {
    let dir = workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let data = home_of(&dir).join("tole");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("config.toml"), CONFIG).unwrap();
    std::fs::write(
        data.join("reviewer.md"),
        "\n  You check finished work.\nSecond line.\n",
    )
    .unwrap();
    skill(&dir, "weekly", "lay out the report");

    let draft: Draft = serde_json::from_value(json!({
        "id": "d-1", "name": "invoice-naming", "description": "name invoices", "status": "pending",
        "created_at": 1, "content_hash": "sha256:00",
        "provenance": {"sessions": ["s"], "trigger": "dead_end", "model": null},
        "taint": ["web_fetch"], "warnings": [],
        "dedup": {"nearest": null, "score": 0.0, "band": "new"}, "promotion": null,
    }))
    .unwrap();
    DraftStore::new(data.join("skill-drafts"))
        .create(&draft, "---\nname: invoice-naming\n---\nbody")
        .unwrap();
    let old = home_of(&dir).join("stash/old-one");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(
        old.join("SKILL.md"),
        "---\nname: old-one\ndescription: \"old\"\n---\nold",
    )
    .unwrap();
    Archive::new(
        data.join("skill-curator-ledger.jsonl"),
        data.join("skill-archive"),
    )
    .archive("old-one", &old, "unused for 30 days", 10)
    .unwrap();

    let marker = dir.join("server-started");
    // `--mcp-server` exists only in a build with the `mcp` feature; without it
    // there is no connected server to configure and nothing that could start.
    #[cfg(feature = "mcp")]
    let args = {
        let script = home_of(&dir).join("fake-server.sh");
        common::write_executable(
            &script,
            format!("#!/bin/sh\ntouch {}\nsleep 5\n", marker.display()),
        );
        vec![
            "--mcp-server".to_string(),
            format!("nginjen={}", script.display()),
        ]
    };
    #[cfg(not(feature = "mcp"))]
    let args: Vec<String> = Vec::new();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // The version check of serve is a background job that writes
    // `update-cache.json` into the data directory when it gets an answer: on a
    // machine with a network it lands in the middle of a test and shows up as a
    // change the requests did not make. This test owns the directory, so the
    // check is off (the switch other serve tests use).
    let serve = Serve::spawn_in_env_args(
        &spawn_mock(0),
        dir,
        true,
        &[("TOLE_NO_UPDATE_CHECK", "1")],
        &args,
    );
    (serve, marker)
}

/// Which files differ between two snapshots, by name and size, so a failure
/// says what changed instead of printing every byte of every file.
fn describe_change(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> String {
    let mut out = Vec::new();
    for name in before
        .keys()
        .chain(after.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        match (before.get(name), after.get(name)) {
            (Some(a), Some(b)) if a != b => {
                out.push(format!("{name} changed ({} to {} bytes)", a.len(), b.len()))
            }
            (Some(_), None) => out.push(format!("{name} removed")),
            (None, Some(b)) => out.push(format!("{name} added ({} bytes)", b.len())),
            _ => {}
        }
    }
    out.join(", ")
}

fn client(serve: &Serve) -> Client {
    let url = format!("http://127.0.0.1:{}", serve.port);
    Client::connect_tcp(&url, Some("t0ken".into()), false).unwrap()
}

fn lossless<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    assert_eq!(&serde_json::to_value(typed).unwrap(), raw, "{what}");
}

/// Every file of the owner's data directory with its bytes, apart from the
/// ticker's heartbeat.
fn snapshot(serve: &Serve) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else if !p.to_string_lossy().contains("ticker.json") {
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let root = home_of(&serve.dir).join("tole");
    let mut out = BTreeMap::new();
    walk(&root, &root, &mut out);
    out
}

#[test]
fn the_profiles_are_display_rows_of_the_owners_config() {
    let (serve, _) = serve();
    let c = client(&serve);
    let list = c.profiles().unwrap();
    let names: Vec<&str> = list.profiles.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["lost", "reviewer"]);
    let p = list.profiles.iter().find(|p| p.name == "reviewer").unwrap();
    assert_eq!(p.persona.as_deref(), Some("You check finished work."));
    assert_eq!(p.memory_room.as_deref(), Some("notes"));
    assert_eq!(p.may_use.as_deref(), Some(&["web_search".to_string()][..]));
    let models = p.models.as_ref().unwrap();
    assert_eq!(models["standard"], "m-std");
    assert_eq!(
        (models["advisor"].as_str(), models["default"].as_str()),
        (Some("m-adv"), Some("standard"))
    );
    // A profile that cannot be loaded is a row with its error, not a failure.
    let lost = list.profiles.iter().find(|p| p.name == "lost").unwrap();
    assert!(lost.error.is_some() && lost.may_use.is_none());
    let (code, raw) = serve.http("GET", "/profiles", None);
    assert_eq!(code, 200);
    lossless("profiles", &list, &raw);
    // The persona beyond its first line and the paths stay in the files.
    assert!(!raw.to_string().contains("Second line"));
}

#[test]
fn the_skills_are_listed_by_state_and_never_trusted_by_reading() {
    let (serve, _) = serve();
    let c = client(&serve);
    let state = c.skills().unwrap();
    let weekly = state.skills.iter().find(|s| s.name == "weekly").unwrap();
    assert_eq!(weekly.state, "not_approved");
    assert_eq!(state.drafts.len(), 1);
    assert!(state.drafts[0].tainted && state.drafts[0].name == "invoice-naming");
    assert_eq!(state.archived.len(), 1);
    assert_eq!(state.archived[0].skill, "old-one");
    let (_, raw) = serve.http("GET", "/skills", None);
    lossless("skills", &state, &raw);

    // The owner approves it at a terminal; a later edit makes it `changed`.
    let hash = weekly.hash.clone().unwrap();
    let mut cmd = common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &home_of(&serve.dir), &[]);
    cmd.args([
        "skills",
        "approve",
        "weekly",
        "--hash",
        &hash,
        "--workspace",
    ])
    .arg(&serve.dir)
    .current_dir(&serve.dir)
    .stdin(Stdio::null())
    .stdout(Stdio::null());
    assert!(cmd.status().unwrap().success());
    let state = c.skills().unwrap();
    let weekly = state.skills.iter().find(|s| s.name == "weekly").unwrap();
    assert_eq!(weekly.state, "approved");
    assert_eq!(weekly.approved_hash.as_deref(), Some(hash.as_str()));
    skill(&serve.dir, "weekly", "lay out the report differently");
    let state = c.skills().unwrap();
    assert_eq!(
        state
            .skills
            .iter()
            .find(|s| s.name == "weekly")
            .unwrap()
            .state,
        "changed"
    );
}

/// #1117: a draft store or an archive ledger that cannot be read is a line in
/// `notes` (the partial marker), not an empty list that reads as "nothing".
#[test]
fn an_unreadable_draft_store_and_archive_are_notes_not_empty_lists() {
    let (serve, _) = serve();
    let c = client(&serve);
    let clean = c.skills().unwrap();
    assert!(clean.notes.is_empty(), "{:?}", clean.notes);

    // Turn the drafts directory into a file and the ledger into a directory:
    // both fail to read for any user, root included.
    let data = home_of(&serve.dir).join("tole");
    std::fs::remove_dir_all(data.join("skill-drafts")).unwrap();
    std::fs::write(data.join("skill-drafts"), "not a directory").unwrap();
    std::fs::remove_file(data.join("skill-curator-ledger.jsonl")).unwrap();
    std::fs::create_dir(data.join("skill-curator-ledger.jsonl")).unwrap();

    let state = c.skills().unwrap();
    assert!(state.drafts.is_empty() && state.archived.is_empty());
    assert_eq!(state.notes.len(), 2, "{:?}", state.notes);
    assert!(state.notes[0].starts_with("drafts not read: "));
    assert!(state.notes[1].starts_with("archive not read: "));
    let (_, raw) = serve.http("GET", "/skills", None);
    lossless("skills", &state, &raw);
}

#[cfg(feature = "mcp")]
#[test]
fn the_tools_state_comes_from_the_lock_and_starts_no_server() {
    let (serve, marker) = serve();
    let c = client(&serve);
    let tools = c.tools().unwrap();
    assert!(!tools.probed);
    let server = tools.servers.iter().find(|s| s.name == "nginjen").unwrap();
    assert_eq!(server.trust, "not_trusted");
    assert!(server.state.starts_with("unapproved"), "{}", server.state);
    let (_, raw) = serve.http("GET", "/tools", None);
    lossless("tools", &tools, &raw);
    // Reading the state is not starting the server, however often it is read.
    for _ in 0..3 {
        c.tools().unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!marker.exists(), "a connected server was started by a read");
}

#[test]
fn no_route_trusts_a_skill_or_tool_changes_the_allow_list_or_edits_a_profile() {
    let (serve, marker) = serve();
    // One read of each first, so anything a read creates is in the snapshot.
    for path in ["/profiles", "/skills", "/tools"] {
        assert_eq!(serve.http("GET", path, None).0, 200, "{path}");
    }
    let before = snapshot(&serve);
    assert!(before.contains_key("config.toml"));
    let body = r#"{"name":"weekly","hash":"sha256:00","allow":["*"],"decision":"allow"}"#;
    for path in [
        "/profiles",
        "/profiles/reviewer",
        "/skills",
        "/skills/weekly",
        "/skills/weekly/approve",
        "/skills/weekly/trust",
        "/skills/drafts/d-1/promote",
        "/tools",
        "/tools/nginjen",
        "/tools/nginjen/approve",
        "/tools/nginjen/revoke",
        "/allow",
        "/config",
    ] {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let (code, v) = serve.http(method, path, Some(body));
            assert!(code == 404 || code == 405, "{method} {path}: {code} {v}");
        }
        // The read routes also take no query that changes anything.
        serve.http("GET", &format!("{path}?approve=1&trust=1"), None);
    }
    let after = snapshot(&serve);
    assert!(
        after == before,
        "a request changed the owner's files: {}",
        describe_change(&before, &after)
    );
    assert!(!marker.exists());
}
