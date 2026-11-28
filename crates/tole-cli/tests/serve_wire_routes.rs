//! #1024: golden JSON of the read routes of `tole serve`, over the REAL serve
//! face with a scripted provider. A client written against today's bodies
//! keeps working only while these files hold: a removed or renamed key, or a
//! value that means something else, fails here and has to be accepted on
//! purpose (see `serve_golden`, and "Wire compatibility" in
//! `docs/mobile-control.md`). A key a newer serve ADDS also shows up as a
//! diff: accepting it is the additive path, and the review sees it.

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};

mod common;
mod serve_golden;
mod serve_harness;
use serve_golden::{check, Volatile};
use serve_harness::{new_session, prompt, spawn_mock, Serve};

fn get(serve: &Serve, path: &str) -> Value {
    let (code, body) = serve.http("GET", path, None);
    assert_eq!(code, 200, "GET {path}: {body}");
    body
}

#[test]
fn read_routes_keep_their_shape() {
    let serve = Serve::spawn(&spawn_mock(0));

    // The state of a server nobody has used yet.
    let fresh = [
        ("health", "/health"),
        ("info", "/info"),
        ("ready", "/ready"),
        ("models", "/models"),
        ("sessions_empty", "/sessions"),
        ("sessions_index_empty", "/sessions/index"),
        ("approvals_empty", "/approvals"),
        ("discussions_empty", "/discussions"),
        ("cron_jobs_empty", "/cron/jobs"),
        ("cron_runs_empty", "/cron/runs"),
        ("profiles_empty", "/profiles"),
        ("skills_empty", "/skills"),
        ("tools", "/tools"),
    ];
    let none = Volatile::of(&serve, &[]);
    let mut seen = Vec::new();
    for (name, path) in fresh {
        let mut v = none.apply(&get(&serve, path));
        if name == "info" && !cfg!(feature = "mcp") {
            // The golden is recorded with the default features: add back the one this build lacks.
            let features = v["features"].as_array_mut().unwrap();
            features.push(json!("mcp"));
            features.sort_by_key(|f| f.as_str().unwrap().to_string());
        }
        seen.push((name, v));
    }

    // One session that answered, one that waits for an approval.
    let idle = new_session(&serve);
    prompt(&serve, &idle, "hello");
    let waiting = new_session(&serve);
    let (code, body) = serve.http(
        "POST",
        &format!("/sessions/{waiting}/prompt"),
        Some(&json!({"text": "TOOL_MISSION write it"}).to_string()),
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["stopReason"], "approval_required", "{body}");
    let approval = body["approval"]["id"].as_str().unwrap().to_string();

    let vol = Volatile::of(
        &serve,
        &[
            (&idle, "<SESSION-A>"),
            (&waiting, "<SESSION-B>"),
            (&approval, "<APPROVAL>"),
        ],
    );
    let used = [
        ("sessions", "/sessions".to_string()),
        ("sessions_index", "/sessions/index".to_string()),
        ("status", format!("/sessions/{idle}/status")),
        ("entries", format!("/sessions/{idle}/entries")),
        (
            "entries_page",
            format!("/sessions/{idle}/entries?after=1&limit=1"),
        ),
        ("tree", format!("/sessions/{idle}/tree")),
        ("approvals", "/approvals".to_string()),
        (
            "approvals_by_session",
            format!("/approvals?session={waiting}"),
        ),
    ];
    for (name, path) in used {
        let mut v = vol.apply(&get(&serve, &path));
        if name == "sessions" {
            // The live map is not ordered; the placeholders are.
            let list = v["sessions"].as_array_mut().unwrap();
            list.sort_by_key(|s| s["id"].as_str().unwrap().to_string());
        }
        seen.push((name, v));
    }
    // `GET /sessions/{id}` is the same answer as `/status` (a documented alias).
    assert_eq!(
        vol.apply(&get(&serve, &format!("/sessions/{idle}"))),
        vol.apply(&get(&serve, &format!("/sessions/{idle}/status"))),
    );
    for (name, v) in &seen {
        check(&format!("routes/{name}"), v);
    }
}

/// The refusals a client branches on: the status and the `{"error"}` body.
#[test]
fn refusals_keep_their_shape() {
    let serve = Serve::spawn(&spawn_mock(0));
    let id = new_session(&serve);
    let asks: [(&str, &str, bool, Option<&str>); 8] = [
        ("GET", "/sessions", false, None),
        ("GET", "/info", false, None),
        ("GET", "/health", false, None),
        ("GET", "/ready", false, None),
        ("GET", "/no/such/route", true, None),
        ("GET", "/sessions/nope/entries", true, None),
        ("GET", "/sessions/{id}/entries?after=x", true, None),
        ("POST", "/sessions/{id}/prompt", true, Some("{}")),
    ];
    let seen: Vec<Value> = asks
        .iter()
        .map(|(method, path, auth, body)| {
            let path = path.replace("{id}", &id);
            let (code, mut v) = serve.http_with(method, &path, *auth, *body);
            // Clients branch on the status and the key; the wording is help text,
            // but for the two words `unauthorized` and `not found`, which they match.
            if let Some(e) = v.get_mut("error") {
                if !matches!(e.as_str(), Some("unauthorized" | "not found")) {
                    assert!(e.is_string(), "{e}");
                    *e = json!("<message>");
                }
            }
            json!({"request": format!("{method} {}", path.replace(&id, "<SESSION>")),
                   "authenticated": auth, "status": code, "body": v})
        })
        .collect();
    let vol = Volatile::of(&serve, &[(&id, "<SESSION>")]);
    check("routes/refusals", &vol.apply(&json!(seen)));
}

/// #1023: the prompt answers that settle a non-final turn carry a `problem`
/// next to `stopReason` (which keeps its value), and a final answer does not.
#[test]
fn prompt_answers_keep_their_shape() {
    let serve = Serve::spawn(&spawn_mock(0));
    let mut sessions = Vec::new();
    let mut seen = Vec::new();
    for (name, text) in [
        ("prompt_final", "hello"),
        ("prompt_approval_required", "TOOL_MISSION write it"),
        ("prompt_provider_failed", "PROVIDER_FAIL go"),
        ("prompt_unknown_tool", "UNKNOWN_TOOL go"),
        ("prompt_loop", "LOOP_TOOL go"),
    ] {
        let id = new_session(&serve);
        let body = json!({"text": text}).to_string();
        let (code, answer) = serve.http("POST", &format!("/sessions/{id}/prompt"), Some(&body));
        assert_eq!(code, 200, "{answer}");
        sessions.push(id);
        seen.push((name, answer));
    }
    let approval = seen[1].1["approval"]["id"].as_str().unwrap().to_string();
    let mut ids: Vec<(&str, &'static str)> = vec![(&approval, "<APPROVAL>")];
    ids.extend(
        sessions
            .iter()
            .map(|s| s.as_str())
            .zip(["<S1>", "<S2>", "<S3>", "<S4>", "<S5>"]),
    );
    let vol = Volatile::of(&serve, &ids);
    let answers: serde_json::Map<String, Value> = seen
        .into_iter()
        .map(|(name, answer)| (name.to_string(), answer))
        .collect();
    check("routes/prompt_answers", &vol.apply(&Value::Object(answers)));
}

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

/// The routes that read what the owner configured: one cron job, two profiles
/// (one cannot be loaded) and one skill that nobody approved.
#[test]
fn owner_state_routes_keep_their_shape() {
    let dir = serve_harness::workspace_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let data = serve_harness::home_of(&dir).join("tole");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("config.toml"), CONFIG).unwrap();
    std::fs::write(
        data.join("reviewer.md"),
        "You check finished work.\nSecond line.\n",
    )
    .unwrap();
    let skill = dir.join("skills/weekly");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: weekly\ndescription: \"weekly skill\"\n---\nlay out the report",
    )
    .unwrap();
    let spec = tole_cli::cron::CreateSpec {
        id: Some("nightly".into()),
        name: Some("Nightly report".into()),
        schedule: "0 9 * * *".into(),
        script: Some("echo hi".into()),
        workdir: dir.clone(),
        ..tole_cli::cron::CreateSpec::default()
    };
    tole_cli::cron::create(&data.join("cron"), spec, tole_cli::cron::now()).unwrap();

    let serve = Serve::spawn_in(&spawn_mock(0), dir, true);
    let vol = Volatile::of(&serve, &[]);
    for (name, path) in [
        ("cron_jobs", "/cron/jobs"),
        ("profiles", "/profiles"),
        ("skills", "/skills"),
    ] {
        check(&format!("routes/{name}"), &vol.apply(&get(&serve, path)));
    }
}
