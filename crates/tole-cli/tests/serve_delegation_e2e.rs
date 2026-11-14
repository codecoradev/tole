//! #714 part 4 end-to-end over the REAL serve face: the durable approval
//! queue of delegated children is listed and decided through `/approvals`
//! with the same bearer token as every other route, refuses under an agent
//! session, and leaves serve's own in-memory queue alone.

#![cfg(feature = "shell-tools")]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tole_cli::delegated_approver::Store;
use tole_core::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Spend,
};
use tole_core::delegation_approvals::{NewRequest, Origin};
use tole_core::ledger::Lease;

mod common;

const T: i64 = 4_000_000_000;

struct Serve {
    child: Child,
    port: u16,
}

impl Serve {
    fn spawn(home: &Path, env: &[(&str, &str)]) -> Self {
        let (child, port) = common::spawn_listening(|port| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_tole"));
            cmd.args(["serve", "--port", &port.to_string(), "--token", "t0ken"])
                .env("CODECORA_HOME", home)
                .env("TOLE_NO_UPDATE_CHECK", "1")
                .env_remove("TOLE_UNDER_SESSION")
                .env_remove("TOLE_AGENT_DEPTH")
                .stdin(Stdio::null())
                .stdout(Stdio::null());
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd
        });
        Self { child, port }
    }

    fn http(&self, method: &str, path: &str, token: bool, body: Option<&str>) -> (u16, Value) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let auth = if token {
            "Authorization: Bearer t0ken\r\n"
        } else {
            ""
        };
        let b = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}",
            b.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        let mut status = String::new();
        r.read_line(&mut status).unwrap();
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut payload = String::new();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            if line == "\r\n" || line == "\n" {
                r.read_to_string(&mut payload).unwrap();
                break;
            }
        }
        (
            code,
            serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
        )
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-sdeleg-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One pending request of a delegated child under `<home>/tole/delegation`.
fn request(home: &Path, secret_in_display: &str) -> (Store, String) {
    let dir = home.join("tole").join("delegation").join("abc");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("delegation.jsonl");
    let mut d = Delegation::open(tole_cli::ledger_file::FileStore::new(log.clone())).unwrap();
    let allow: Vec<String> = vec!["write_file".into(), "delegate_decide".into()];
    let goal = Goal {
        id: "g",
        task: "t",
        budget: Budget {
            tokens: 1000,
            steps: 100,
            deadline: T + 7200,
        },
        allow: &allow,
        max_depth: 2,
        max_live: 8,
    };
    d.begin(&goal, T).unwrap();
    let r = DelegateRequest {
        profile: "coder",
        task: "t",
        mode: Mode::Execute,
        budget: BudgetRequest::default(),
        profile_allow: &allow,
        workspace: None,
    };
    let node = d.delegate("g", &r, Spend::default(), T).unwrap().node_id;
    let lease = Lease {
        pid: 7,
        start_time: 1,
    };
    d.start(&node, lease, Some("sess-kid"), T).unwrap();
    let store = Store::new(log);
    let input = json!({"path": "src/a.rs"});
    // The server reads the clock itself, so the request must still be open.
    let now = tole_cli::delegated_approver::real_now();
    let id = store
        .write(|d, a| {
            let n = NewRequest {
                node: &node,
                session: "sess-kid",
                tool: "write_file",
                origin: Origin::Builtin,
                target: Some("src/a.rs"),
                raw_input: &input,
                input: &input,
                display: secret_in_display,
                secrets: false,
                protected: false,
                workspace: "/w",
                approval_secs: 900,
                level_secs: 120,
                nonce: "aaaaaaaa",
            };
            a.request(d, &n, now)
        })
        .unwrap();
    (store, id)
}

#[test]
fn the_queue_is_listed_and_decided_with_the_serve_token_and_nothing_else() {
    let h = home("main");
    let (store, id) = request(&h, "write src/a.rs");
    let s = Serve::spawn(&h, &[]);
    // The same bearer rule as every route.
    assert_eq!(s.http("GET", "/approvals", false, None).0, 401);
    let (code, list) = s.http("GET", "/approvals", true, None);
    assert_eq!(code, 200, "{list}");
    let a = list["approvals"].as_array().unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(
        (a[0]["id"].as_str(), a[0]["source"].as_str()),
        (Some(id.as_str()), Some("delegation"))
    );
    let path = format!("/approvals/{id}/decision");
    let body = json!({"decision": "allow", "confirm": id}).to_string();
    assert_eq!(s.http("POST", &path, false, Some(&body)).0, 401);
    assert!(store
        .read(|_, a| a.state().entry(&id).unwrap().decision.is_none())
        .unwrap());
    // Allow without the id repeated: refused. With it: decided via serve.
    let (code, _) = s.http("POST", &path, true, Some(r#"{"decision":"allow"}"#));
    assert_eq!(code, 400);
    let (code, v) = s.http("POST", &path, true, Some(&body));
    assert_eq!((code, v["status"].as_str()), (200, Some("allowed")), "{v}");
    let via = store
        .read(|_, a| a.state().entry(&id).unwrap().decision.clone().unwrap().via)
        .unwrap();
    assert_eq!(via, tole_core::delegation_approvals::Via::Serve);
    // No longer pending: gone from the list, and not decidable twice.
    let (_, list) = s.http("GET", "/approvals", true, None);
    assert!(list["approvals"].as_array().unwrap().is_empty());
    assert_eq!(s.http("POST", &path, true, Some(&body)).0, 409);
    // Serve's own queue is untouched: its ids still resolve there only.
    let (code, v) = s.http(
        "POST",
        "/approvals/apr-1-abc/decision",
        true,
        Some(r#"{"decision":"deny"}"#),
    );
    assert_eq!(
        (code, v["error"].as_str()),
        (404, Some("unknown approval id"))
    );
}

#[test]
fn a_serve_running_under_an_agent_session_refuses_to_decide() {
    let h = home("session");
    let (store, id) = request(&h, "write src/a.rs");
    let s = Serve::spawn(&h, &[("TOLE_UNDER_SESSION", "1")]);
    let path = format!("/approvals/{id}/decision");
    let body = json!({"decision": "deny"}).to_string();
    let (code, v) = s.http("POST", &path, true, Some(&body));
    assert_eq!(code, 403, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("agent session"),
        "{v}"
    );
    assert!(store
        .read(|_, a| a.state().entry(&id).unwrap().decision.is_none())
        .unwrap());
    // Reading is not an owner decision.
    assert_eq!(s.http("GET", "/approvals", true, None).0, 200);
}

#[test]
fn without_delegation_approvals_the_list_is_serves_own_and_unchanged() {
    let h = home("off");
    let s = Serve::spawn(&h, &[]);
    let (code, v) = s.http("GET", "/approvals", true, None);
    assert_eq!((code, v), (200, json!({"approvals": []})));
}
