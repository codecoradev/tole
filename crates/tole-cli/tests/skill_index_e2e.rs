//! #590 end to end with the REAL binary: `TOLE_SKILL_INDEX=uteke` lists
//! the pinned core plus the skills a (fake) uteke index recalls for the
//! `run` prompt, and the default lists every skill as before. The mock
//! provider answers with the system message it received.
#![cfg(all(unix, feature = "shell-tools"))]

mod common;
use common::TestDir;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Mock: answers every request with `SYSTEM::<the system message>`.
fn spawn_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = stream;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 16384];
                let mut body = Value::Null;
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if let Ok(v) = serde_json::from_slice::<Value>(&buf[h + 4..]) {
                            body = v;
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let system = body["messages"][0]["content"].as_str().unwrap_or("?");
                let content = format!("SYSTEM::{system}");
                let resp = if body["stream"].as_bool() == Some(true) {
                    let mut out = String::new();
                    for part in [content.as_str(), ""] {
                        out.push_str(&format!(
                            "data: {}\n\n",
                            json!({"choices": [{"index": 0,
                                "delta": {"content": part}, "finish_reason": null}]})
                        ));
                    }
                    out.push_str("data: [DONE]\n\n");
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        out.len(),
                        out
                    )
                } else {
                    let data = json!({
                        "id": "c", "object": "chat.completion", "created": 1, "model": "mock",
                        "choices": [{"index": 0, "message": {"role": "assistant",
                            "content": content}, "finish_reason": "stop"}]
                    })
                    .to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        data.len(),
                        data
                    )
                };
                let _ = s.write_all(resp.as_bytes());
            });
        }
    });
    format!("http://{addr}/v1")
}

/// A workspace with four skills and a fake `uteke` whose semantic search
/// returns `beta` (0.9), `gamma` (0.2, below the floor) and `delta` (0.8,
/// retired). It logs every call.
fn fixture(tag: &str) -> TestDir {
    fixture_with(tag, false)
}

/// `pre_024`: a uteke without the `skill` subcommand (it exits 2 on it).
fn fixture_with(tag: &str, pre_024: bool) -> TestDir {
    let dir = TestDir::new(&format!("tole-skidx-e2e-{tag}"));
    for n in ["alpha", "beta", "gamma", "delta"] {
        let d = dir.join("ws").join("skills").join(n);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {n}\ndescription: \"local {n}\"\n---\nbody"),
        )
        .unwrap();
    }
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let doc = |n: &str, status: &str| {
        json!({"slug": format!("skill-{n}"), "tags": ["skill"],
               "metadata": {"name": n, "status": status}})
        .to_string()
    };
    for (n, st) in [
        ("beta", "approved"),
        ("gamma", "approved"),
        ("delta", "retired"),
    ] {
        std::fs::write(bin.join(format!("skill-{n}.json")), doc(n, st)).unwrap();
    }
    let hits = json!([
        {"document": {"slug": "skill-beta"}, "score": 0.9},
        {"document": {"slug": "skill-delta"}, "score": 0.8},
        {"document": {"slug": "skill-gamma"}, "score": 0.2},
    ]);
    std::fs::write(bin.join("search.json"), hits.to_string()).unwrap();
    let skill_hits = json!([
        {"name": "beta", "score": 0.9},
        {"name": "delta", "score": 0.8, "status": "retired"},
        {"name": "gamma", "score": 0.2},
    ]);
    std::fs::write(bin.join("skill-search.json"), skill_hits.to_string()).unwrap();
    let skill_help = if pre_024 {
        "echo 'unrecognized subcommand' >&2; exit 2"
    } else {
        "exit 0"
    };
    let b = bin.display();
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> {b}/argv.log\n\
         case \"$1 $2\" in\n\
         'skill --help') {skill_help} ;;
         'skill search') cat {b}/skill-search.json ;;
         'doc search') cat {b}/search.json ;;\n\
         'doc get') cat {b}/$3.json ;;\n\
         *) echo '[]' ;;\n\
         esac\n"
    );
    common::write_executable(bin.join("uteke"), script);
    dir
}

fn run(dir: &Path, url: &str, envs: &[(&str, &str)]) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
    cmd.args([
        "run",
        "--workspace",
        dir.join("ws").to_str().unwrap(),
        "-s",
        dir.join("sessions").to_str().unwrap(),
        "ship the release",
    ])
    .env("TOLE_BASE_URL", url)
    .env("TOLE_MODEL", "m")
    .env("TOLE_API_KEY", "k")
    .env("TOLE_NO_UPDATE_CHECK", "1")
    .env("CODECORA_HOME", dir.join("home"))
    .env(
        "PATH",
        format!("{}:/usr/bin:/bin", dir.join("bin").display()),
    )
    .env_remove("TOLE_FALLBACK_BASE_URL")
    .env_remove("TOLE_MEMORY")
    .env_remove("TOLE_SYSTEM_PROMPT")
    .env_remove("TOLE_TRUST")
    .env_remove("TOLE_SKILL_INDEX")
    .env_remove("TOLE_SKILLS_PINNED")
    .env_remove("TOLE_SKILL_RECALL_K")
    .env_remove("TOLE_SKILL_FLOOR")
    // These tests are about the index; skill approval (required by
    // default) is opted out explicitly.
    .env("TOLE_SKILL_APPROVAL", "off");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run tole");
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn uteke_index_lists_pinned_plus_recalled_for_the_run_prompt() {
    let url = spawn_mock();
    let dir = fixture("uteke");
    let (out, err, code) = run(
        &dir,
        &url,
        &[
            ("TOLE_SKILL_INDEX", "uteke"),
            ("TOLE_SKILLS_PINNED", "alpha"),
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("- alpha — local alpha\n- beta — local beta\n"),
        "{out}\n{err}"
    );
    assert!(!out.contains("- gamma"), "below the floor: {out}");
    assert!(!out.contains("- delta"), "retired: {out}");
    assert!(out.contains("skill_search"), "{out}");
    // The recall query is the run prompt, sent to the skill surface (#973),
    // never to the hidden-in-0.24 document surface.
    let log = std::fs::read_to_string(dir.join("bin/argv.log")).unwrap();
    assert!(
        log.contains("skill search ship the release --limit"),
        "{log}"
    );
    assert!(!log.contains("doc search"), "{log}");
    assert!(!err.contains("no `skill` subcommand"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pre_024_uteke_degrades_to_the_doc_surface_with_one_note() {
    let url = spawn_mock();
    let dir = fixture_with("pre024", true);
    let (out, err, code) = run(
        &dir,
        &url,
        &[
            ("TOLE_SKILL_INDEX", "uteke"),
            ("TOLE_SKILLS_PINNED", "alpha"),
        ],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("- alpha — local alpha\n- beta — local beta\n"),
        "{out}\n{err}"
    );
    assert!(!out.contains("- delta"), "retired: {out}");
    assert_eq!(
        out.matches("no `skill` subcommand").count() + err.matches("no `skill` subcommand").count(),
        1,
        "exactly one note: {out}\n{err}"
    );
    let log = std::fs::read_to_string(dir.join("bin/argv.log")).unwrap();
    assert!(
        log.contains("doc search ship the release --mode semantic"),
        "{log}"
    );
    assert!(!log.contains("skill search"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn default_index_lists_every_skill_and_never_calls_uteke_for_skills() {
    let url = spawn_mock();
    let dir = fixture("local");
    let (out, err, code) = run(&dir, &url, &[]);
    assert_eq!(code, 0, "{err}");
    for n in ["alpha", "beta", "gamma", "delta"] {
        assert!(out.contains(&format!("- {n} — local {n}")), "{out}");
    }
    let log = std::fs::read_to_string(dir.join("bin/argv.log")).unwrap_or_default();
    assert!(!log.contains("doc search"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_invalid_setting_fails_startup() {
    let url = spawn_mock();
    let dir = fixture("bad");
    let (_, err, code) = run(&dir, &url, &[("TOLE_SKILL_RECALL_K", "50")]);
    assert_ne!(code, 0);
    assert!(err.contains("TOLE_SKILL_RECALL_K"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
