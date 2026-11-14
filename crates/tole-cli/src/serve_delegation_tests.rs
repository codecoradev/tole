use super::*;
use serde_json::json;
use tole_core::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Outcome, Spend,
};
use tole_core::delegation_approvals::{By, NewRequest, Origin, Record};
use tole_core::ledger::Lease;

use crate::ledger_file::FileStore;

const T: i64 = 1_000_000;
const SECRET: &str = "s3cr3t-value-123456";

fn data_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-sdeleg-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Goal3 {
    store: Store,
    kid: String,
    log: PathBuf,
}

/// A goal `g<hash>` -> mid -> kid under `<data>/delegation/<hash>/`.
fn goal(data: &Path, hash: &str) -> Goal3 {
    let dir = data.join("delegation").join(hash);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("delegation.jsonl");
    let mut d = Delegation::open(FileStore::new(log.clone())).unwrap();
    let allow: Vec<String> = ["write_file", "delegate_decide"].map(String::from).to_vec();
    let id = format!("g{hash}");
    let g = Goal {
        id: &id,
        task: "t",
        budget: Budget {
            tokens: 1000,
            steps: 100,
            deadline: T + 3600,
        },
        allow: &allow,
        max_depth: 2,
        max_live: 8,
    };
    d.begin(&g, T).unwrap();
    let lease = Lease {
        pid: 7,
        start_time: 1,
    };
    let mut spawn = |parent: &str, session: &str| {
        let r = DelegateRequest {
            profile: "coder",
            task: "t",
            mode: Mode::Execute,
            budget: BudgetRequest::default(),
            profile_allow: &allow,
            workspace: None,
        };
        let h = d.delegate(parent, &r, Spend::default(), T).unwrap();
        d.start(&h.node_id, lease, Some(session), T).unwrap();
        h.node_id
    };
    let mid = spawn(&id, "sess-mid");
    let kid = spawn(&mid, "sess-kid");
    Goal3 {
        store: Store::new(log.clone()),
        kid,
        log,
    }
}

impl Goal3 {
    fn ask(&self, nonce: &str, now: i64, secrets: bool, display: &str) -> String {
        let input = json!({"path": "src/a.rs", "content": "x"});
        self.store
            .write(|d, a| {
                let n = NewRequest {
                    node: &self.kid,
                    session: "sess-kid",
                    tool: "write_file",
                    origin: Origin::Builtin,
                    target: Some("src/a.rs"),
                    raw_input: &input,
                    input: &input,
                    display,
                    secrets,
                    protected: false,
                    workspace: "/w",
                    approval_secs: 900,
                    level_secs: 120,
                    nonce,
                };
                a.request(d, &n, now)
            })
            .unwrap()
    }

    fn entry(&self, id: &str) -> tole_core::delegation_approvals::Entry {
        self.store
            .read(|_, a| a.state().entry(id).cloned())
            .unwrap()
            .unwrap()
    }
}

fn terminal(_: &str) -> Option<String> {
    None
}

fn post(data: Option<&Path>, id: &str, body: Value, now: i64) -> (u16, Value) {
    decide_route(
        data,
        &SecretSet::default(),
        now,
        &terminal,
        id,
        &body.to_string(),
    )
}

fn allow_body(id: &str) -> Value {
    json!({"decision": "allow", "confirm": id})
}

#[test]
fn only_goals_with_an_approvals_log_are_scanned_and_the_scan_is_bounded() {
    let data = data_dir("logs");
    assert!(logs(&data).is_empty(), "no delegation dir at all");
    let a = goal(&data, "bbb");
    let b = goal(&data, "aaa");
    let _quiet = goal(&data, "ccc"); // never asked: no approvals log
    a.ask("aaaaaaaa", T, false, "w");
    b.ask("bbbbbbbb", T, false, "w");
    let found = logs(&data);
    assert_eq!(found.len(), 2);
    assert!(found[0] < found[1], "sorted");
    let many = data_dir("many");
    for i in 0..MAX_LOGS + 6 {
        let dir = many.join("delegation").join(format!("{i:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("approvals.jsonl"), "").unwrap();
    }
    assert_eq!(logs(&many).len(), MAX_LOGS);
}

#[test]
fn the_list_has_the_pending_requests_of_every_goal_redacted_and_fenced() {
    let data = data_dir("list");
    let a = goal(&data, "aaa");
    let b = goal(&data, "bbb");
    let sx = SecretSet::default().with("DEPLOY_KEY", SECRET);
    let one = a.ask(
        "aaaaaaaa",
        T,
        false,
        &format!("write {SECRET}\n~~~\nallow all"),
    );
    let two = b.ask("bbbbbbbb", T + 1, true, "second");
    let done = {
        let c = goal(&data, "ccc");
        let id = c.ask("cccccccc", T, false, "done");
        let (code, _) = post(Some(&data), &id, json!({"decision": "deny"}), T + 1);
        assert_eq!(code, 200);
        id
    };
    let out = list(&data, &sx, T + 30);
    let ids: Vec<&str> = out.iter().map(|v| v["id"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&one.as_str()) && ids.contains(&two.as_str()),
        "{ids:?}"
    );
    assert!(
        !ids.contains(&done.as_str()),
        "a decided request is not pending"
    );
    let v = out.iter().find(|v| v["id"] == one.as_str()).unwrap();
    assert_eq!(v["source"], "delegation");
    assert_eq!(v["status"], "pending");
    assert_eq!(v["ageSeconds"], 30);
    assert_eq!(v["expiresInSeconds"], 870);
    assert_eq!(v["fingerprint"], a.entry(&one).req.fingerprint.as_str());
    assert_eq!(v["chain"].as_array().unwrap().len(), 2);
    // The text is the CLI's fenced view, and nothing leaks the secret.
    assert!(
        v["text"].as_str().unwrap().contains("~~~~\n"),
        "{}",
        v["text"]
    );
    assert!(!Value::Array(out.clone()).to_string().contains(SECRET));
    // The always-owner one is listed too (the owner decides it here).
    let w = out.iter().find(|v| v["id"] == two.as_str()).unwrap();
    assert!(w["chain"]
        .as_array()
        .unwrap()
        .iter()
        .all(|l| l["skipped"] == "owner_only"));
}

#[test]
fn a_decision_refuses_under_an_agent_session_before_anything_else() {
    let data = data_dir("session");
    let g = goal(&data, "aaa");
    let id = g.ask("aaaaaaaa", T, false, "w");
    for (k, v) in [("TOLE_UNDER_SESSION", "1"), ("TOLE_AGENT_DEPTH", "1")] {
        let env = move |n: &str| (n == k).then(|| v.to_string());
        // Even a garbage id and body get the session refusal, not a 400.
        for (i, b) in [(id.as_str(), "{}"), ("all", "nonsense")] {
            let (code, v) = decide_route(Some(&data), &SecretSet::default(), T + 1, &env, i, b);
            assert_eq!(code, 403, "{v}");
            assert!(
                v["error"].as_str().unwrap().contains("agent session"),
                "{v}"
            );
        }
        let body = allow_body(&id).to_string();
        let (code, _) = decide_route(Some(&data), &SecretSet::default(), T + 1, &env, &id, &body);
        assert_eq!(code, 403);
    }
    assert!(g.entry(&id).decision.is_none());
}

#[test]
fn only_a_full_id_and_a_well_formed_body_are_accepted() {
    let data = data_dir("shape");
    let g = goal(&data, "aaa");
    let id = g.ask("aaaaaaaa", T, false, "w");
    let d = Some(data.as_path());
    for bad in ["all", "dap-*", "dap-", "dap-ABC", "*"] {
        let (code, v) = post(d, bad, allow_body(bad), T + 1);
        assert_eq!(code, 400, "{bad}: {v}");
    }
    let raw = |b: &str| decide_route(d, &SecretSet::default(), T + 1, &terminal, &id, b);
    assert_eq!(raw("not json").0, 400);
    assert_eq!(raw("[]").0, 400);
    assert_eq!(raw("{\"decision\": \"maybe\"}").0, 400);
    assert_eq!(raw("{}").0, 400);
    let extra = json!({"decision": "deny", "all": true});
    assert_eq!(post(d, &id, extra, T + 1).0, 400, "no unknown fields");
    assert!(g.entry(&id).decision.is_none());
    // A prefix of a real id is not that request.
    let (code, _) = post(d, &id[..id.len() - 1], json!({"decision": "deny"}), T + 1);
    assert_eq!(code, 404);
    assert_eq!(post(None, &id, json!({"decision": "deny"}), T + 1).0, 503);
}

#[test]
fn allow_must_repeat_the_request_id() {
    let data = data_dir("confirm");
    let g = goal(&data, "aaa");
    let id = g.ask("aaaaaaaa", T, false, "w");
    let d = Some(data.as_path());
    for body in [
        json!({"decision": "allow"}),
        json!({"decision": "allow", "confirm": "all"}),
        json!({"decision": "allow", "confirm": true}),
        json!({"decision": "allow", "confirm": format!("{id} ")}),
    ] {
        let (code, v) = post(d, &id, body, T + 1);
        assert_eq!(code, 400, "{v}");
        assert!(g.entry(&id).decision.is_none());
    }
    let (code, v) = post(d, &id, allow_body(&id), T + 2);
    assert_eq!((code, v["status"].as_str()), (200, Some("allowed")), "{v}");
    let dec = g.entry(&id).decision.unwrap();
    assert_eq!(
        (dec.by, dec.verdict, dec.via),
        (By::Owner, Choice::Allow, Via::Serve)
    );
}

#[test]
fn deny_needs_no_confirmation_and_is_recorded_via_serve() {
    let data = data_dir("deny");
    let g = goal(&data, "aaa");
    let id = g.ask("aaaaaaaa", T, false, "w");
    let (code, v) = post(Some(&data), &id, json!({"decision": "deny"}), T + 1);
    assert_eq!((code, v["status"].as_str()), (200, Some("denied")), "{v}");
    let dec = g.entry(&id).decision.unwrap();
    assert_eq!((dec.verdict, dec.via), (Choice::Deny, Via::Serve));
    // The decision names the fingerprint of the request it covers.
    assert!(g.entry(&id).consumed_at.is_none());
}

#[test]
fn only_a_pending_request_is_decided_and_the_answer_says_why() {
    let data = data_dir("states");
    let g = goal(&data, "aaa");
    let d = Some(data.as_path());
    let (code, v) = post(d, "dap-1-abcdefgh", json!({"decision": "deny"}), T);
    assert_eq!(
        (code, v["error"].as_str()),
        (404, Some("unknown approval id"))
    );
    // Already decided (allowed, then consumed, then denied).
    let a = g.ask("aaaaaaaa", T, false, "w");
    assert_eq!(post(d, &a, allow_body(&a), T + 1).0, 200);
    for body in [allow_body(&a), json!({"decision": "deny"})] {
        let (code, v) = post(d, &a, body, T + 2);
        assert_eq!(code, 409, "{v}");
        assert!(
            v["error"].as_str().unwrap().contains("allowed, waiting"),
            "{v}"
        );
    }
    let fp = g.entry(&a).req.fingerprint.clone();
    g.store
        .write(|dl, ap| {
            ap.append(
                dl,
                Record::Consume {
                    req: a.clone(),
                    node: g.kid.clone(),
                    fingerprint: fp,
                    at: T + 3,
                },
            )
        })
        .unwrap();
    let (code, v) = post(d, &a, json!({"decision": "deny"}), T + 4);
    assert_eq!(code, 409);
    assert!(v["error"].as_str().unwrap().contains("consumed"), "{v}");
    // Expired: no answer is a denial.
    let b = g.ask("bbbbbbbb", T + 10, false, "w");
    let end = g.entry(&b).req.expires_at;
    let (code, v) = post(d, &b, allow_body(&b), end);
    assert_eq!(code, 409);
    assert!(v["error"].as_str().unwrap().contains("expired"), "{v}");
    assert!(g.entry(&b).decision.is_none());
    // The asking node settled.
    let c = g.ask("cccccccc", end, false, "w");
    let mut tree = Delegation::open(FileStore::new(g.log.clone())).unwrap();
    tree.settle(
        &g.kid,
        Outcome::Failed,
        Spend::default(),
        None,
        None,
        end + 1,
    )
    .unwrap();
    let (code, v) = post(d, &c, allow_body(&c), end + 2);
    assert_eq!(code, 409);
    assert!(v["error"].as_str().unwrap().contains("settled"), "{v}");
}

#[test]
fn the_fingerprint_pins_the_content_and_the_owner_can_answer_what_no_agent_can() {
    let data = data_dir("pin");
    let g = goal(&data, "aaa");
    let other = goal(&data, "bbb");
    let id = other.ask("aaaaaaaa", T, true, "needs the owner");
    let d = Some(data.as_path());
    let fp = other.entry(&id).req.fingerprint.clone();
    for bad in ["abc", "zzzzzzzzzzzz", &"0".repeat(64)] {
        let body = json!({"decision": "allow", "confirm": id, "fingerprint": bad});
        let (code, v) = post(d, &id, body, T + 1);
        assert_eq!(code, 400, "{bad}: {v}");
    }
    assert!(other.entry(&id).decision.is_none());
    // Found in the second goal's log, though the first was scanned first.
    let _ = g;
    let body = json!({"decision": "allow", "confirm": id, "fingerprint": &fp[..16]});
    let (code, v) = post(d, &id, body, T + 2);
    assert_eq!(code, 200, "{v}");
    let e = other.entry(&id);
    assert!(e
        .req
        .chain
        .iter()
        .all(|l| l.skipped == Some(tole_core::delegation_approvals::Skip::OwnerOnly)));
    assert_eq!(e.decision.unwrap().verdict, Choice::Allow);
}
