//! #1040 contract test: the typed `tole-client` against the REAL serve face
//! (the `tole` binary, a scripted provider mock).
//!
//! The client's types are its own, so drift is caught here: a response that
//! the client decodes must serialize back to exactly the JSON the server sent
//! (a field the client does not model, or models under another name, fails).
//!
//! The runs are written once and go over both transports: loopback TCP
//! (`tole serve --port`) and, on Unix, the local socket (`tole serve --socket`
//! with the token on an inherited descriptor, #1067).

// Spawns the real `tole serve` binary (`shell-tools` feature, #349).
#![cfg(feature = "shell-tools")]

use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;
use tole_client::{
    Client, Decision, EffortRequest, Error, Event, ForkRequest, IndexQuery, InterruptRequest,
    MissionRequest, ModelRequest, NameRequest, NewSession, PromptRequest, ReconnectPolicy, Route,
    StreamItem,
};

mod common;
mod serve_harness;
use serve_harness::{spawn_mock, Serve};

const WAIT: Duration = Duration::from_secs(20);

type Http = Box<dyn Fn(&str, &str, Option<&str>) -> (u16, Value)>;

/// A running serve and the ways to reach it, whichever transport it listens on.
struct Target {
    /// The typed client under test, with the right token.
    client: Client,
    /// The same transport with a token the server does not know.
    wrong: Client,
    /// A hand-written request over the same transport, for the raw JSON the
    /// typed answer is compared with: `(method, path, body)`.
    http: Http,
    /// The workspace, where an approved write lands.
    workspace: PathBuf,
    /// Keeps the process and its directories alive.
    _serve: Box<dyn std::any::Any>,
}

impl Target {
    fn raw(&self, method: &str, path: &str, body: Option<&str>) -> Value {
        let (code, v) = (self.http)(method, path, body);
        assert_eq!(code, 200, "{method} {path}: {v}");
        v
    }
}

fn tcp_target() -> Target {
    tcp_target_slow(0)
}

/// A serve over a provider mock that holds every request mentioning "slow"
/// for `slow_ms`, so a turn or a mission stays running while the test acts.
fn tcp_target_slow(slow_ms: u64) -> Target {
    tcp_target_with(&spawn_mock(slow_ms))
}

/// A serve over a provider mock that holds a request at a gate (#812): a
/// request whose body contains a marker waits until the test opens that gate.
fn tcp_target_gated(gates: Vec<(&'static str, std::sync::Arc<common::Gate>)>) -> Target {
    tcp_target_with(&serve_harness::spawn_mock_gated(0, gates))
}

fn tcp_target_with(base: &str) -> Target {
    let serve = Rc::new(Serve::spawn(base));
    let url = format!("http://127.0.0.1:{}", serve.port);
    let client = |token: &str| Client::connect_tcp(&url, Some(token.into()), false).unwrap();
    let s = Rc::clone(&serve);
    Target {
        client: client("t0ken"),
        wrong: client("bad"),
        http: Box::new(move |m, p, b| s.http(m, p, b)),
        workspace: serve.dir.clone(),
        _serve: Box::new(serve),
    }
}

#[cfg(unix)]
mod socket {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    use std::time::Duration;

    pub const TOKEN: &str = "t0ken-1067-per-launch";

    /// `tole serve --socket` over a fresh workspace. The token is written to a
    /// pipe whose read end is serve's stdin (`--token-fd 0`), the way the
    /// desktop will start it: never in the arguments or the environment.
    pub struct SocketServe {
        pub socket: PathBuf,
        pub workspace: PathBuf,
        // Declared before `_dir`: the process is gone before its directory.
        _child: common::ChildGuard,
        _dir: common::TestDir,
    }

    impl SocketServe {
        pub fn spawn() -> Self {
            let base = spawn_mock(0);
            // A short path: a socket address holds about 100 bytes.
            let dir = common::TestDir::in_system_temp("cc");
            let workspace = dir.join("ws");
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::create_dir_all(dir.join("home")).unwrap();
            let socket = dir.join("run").join("s.sock");
            let (child, _) = common::spawn_listening(|_| {
                let (read, mut write) = std::io::pipe().unwrap();
                write.write_all(TOKEN.as_bytes()).unwrap();
                drop(write);
                let mut cmd =
                    common::hermetic_command(env!("CARGO_BIN_EXE_tole"), &dir.join("home"), &[]);
                cmd.args(["serve", "--socket"])
                    .arg(&socket)
                    .args(["--token-fd", "0", "--workspace"])
                    .arg(&workspace)
                    .current_dir(&workspace)
                    .env("TOLE_BASE_URL", &base)
                    .env("TOLE_MODEL", "mock-model")
                    .env("TOLE_API_KEY", "sk-test")
                    .env_remove("TOLE_SERVE_TOKEN")
                    .stdin(Stdio::from(read))
                    .stdout(Stdio::null());
                cmd
            });
            SocketServe {
                socket,
                workspace,
                _child: common::ChildGuard(child),
                _dir: dir,
            }
        }
    }

    /// One request, `Connection: close`, over the socket.
    fn http(socket: &PathBuf, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut s = UnixStream::connect(socket).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let body = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        let (head, payload) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
        let code = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (
            code,
            serde_json::from_str(payload.trim()).unwrap_or(Value::Null),
        )
    }

    pub fn target() -> Target {
        let serve = SocketServe::spawn();
        let client = |token: &str| Client::connect_unix(&serve.socket, Some(token.into())).unwrap();
        let socket = serve.socket.clone();
        Target {
            client: client(TOKEN),
            wrong: client("bad"),
            http: Box::new(move |m, p, b| http(&socket, m, p, b)),
            workspace: serve.workspace.clone(),
            _serve: Box::new(serve),
        }
    }
}

/// `typed` is what the client made of the server's answer; it must carry all
/// of `raw` and nothing else.
fn lossless<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    assert_eq!(&serde_json::to_value(typed).unwrap(), raw, "{what}");
}

fn is_final_answer(e: &Event) -> bool {
    matches!(e, Event::Entry(e) if e.payload["text"] == "ENTRIES-ANSWER")
}

fn create_prompt_stream_and_entries_round_trip_through_the_client(t: &Target) {
    let c = &t.client;

    let health = c.health().unwrap();
    // The protocol and the binary version serve reports (#1043) are decoded,
    // not dropped.
    assert_eq!(
        health.protocol,
        Some(tole_cli::protocol::SERVE_PROTOCOL_VERSION)
    );
    assert_eq!(health.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    lossless("health", &health, &t.raw("GET", "/health", None));
    let models = c.models().unwrap();
    assert_eq!(models.default.as_deref(), Some("mock-model"));
    lossless("models", &models, &t.raw("GET", "/models", None));

    let opened = c.create_session(&NewSession::default()).unwrap();
    let id = opened.session_id.clone();
    assert!(opened.workspace.is_some() && opened.resumed.is_none());
    let list = c.sessions().unwrap();
    assert!(list.sessions.iter().any(|s| s.id == id && !s.busy));
    lossless("sessions", &list, &t.raw("GET", "/sessions", None));

    // Open the stream and wait for `ready`: only then does the server attach
    // an observer to the turn (a stream nobody reads costs nothing).
    let (tx, rx) = mpsc::channel();
    let follower = c.clone();
    let sid = id.clone();
    std::thread::spawn(move || {
        for item in follower.follow_events(&sid, 0, ReconnectPolicy::default()) {
            if tx.send(item).is_err() {
                return;
            }
        }
    });
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(WAIT).expect("a ready frame").unwrap() {
            StreamItem::Event(Event::Ready(r)) => {
                assert_eq!((r.id.as_str(), r.after, r.busy), (id.as_str(), 0, false));
                break;
            }
            other => seen.push(other),
        }
    }

    let reply = c.prompt(&id, &PromptRequest::text("hello")).unwrap();
    assert_eq!(reply.text.as_deref(), Some("ENTRIES-ANSWER"), "{reply:?}");
    assert!(reply.approval.is_none());

    // The text delta arrives before the durable final answer; entries come in
    // seq order, each once.
    let mut events = Vec::new();
    loop {
        match rx
            .recv_timeout(WAIT)
            .expect("the final answer entry")
            .unwrap()
        {
            StreamItem::Event(e) => {
                let done = is_final_answer(&e);
                events.push(e);
                if done {
                    break;
                }
            }
            other => seen.push(other),
        }
    }
    let delta = events
        .iter()
        .position(|e| matches!(e, Event::TextDelta { text } if text == "ENTRIES-ANSWER"));
    assert!(delta.is_some_and(|d| d < events.len() - 1), "{events:?}");
    let seqs: Vec<u64> = events
        .iter()
        .filter_map(|e| {
            if let Event::Entry(e) = e {
                Some(e.seq)
            } else {
                None
            }
        })
        .collect();
    assert!(
        seqs.len() >= 2 && seqs.windows(2).all(|w| w[0] < w[1]),
        "{seqs:?}"
    );

    // The transcript read agrees with the stream.
    let page = c.entries(&id, 0, None).unwrap();
    assert_eq!(page.entries.iter().map(|e| e.seq).collect::<Vec<_>>(), seqs);
    lossless(
        "entries",
        &page,
        &t.raw("GET", &format!("/sessions/{id}/entries"), None),
    );
    let first = c.entries(&id, 0, Some(1)).unwrap();
    assert!(first.has_more && first.entries.len() == 1);
    assert!(c
        .entries(&id, page.next_after, None)
        .unwrap()
        .entries
        .is_empty());

    let status = c.status(&id).unwrap();
    assert!(!status.busy && status.entries == Some(page.entries.len() as u64));
    // `snapshotAt` moves between reads: compare the field set, not the values.
    let raw_status = t.raw("GET", &format!("/sessions/{id}/status"), None);
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys(&serde_json::to_value(&status).unwrap()).len(),
        keys(&raw_status).len()
    );
    for k in keys(&raw_status) {
        assert!(
            serde_json::to_value(&status).unwrap().get(&k).is_some(),
            "status lost `{k}`"
        );
    }

    let index = c.session_index(&IndexQuery::default()).unwrap();
    assert!(index
        .sessions
        .iter()
        .any(|s| s.id == id && s.live && s.turns >= 1));
    lossless("index", &index, &t.raw("GET", "/sessions/index", None));

    // Resume of a live session; cancel of an idle one.
    let again = c.resume_session(&id).unwrap();
    assert_eq!(
        (again.already_live, again.resumed),
        (Some(true), Some(false))
    );
    lossless(
        "resume",
        &again,
        &t.raw(
            "POST",
            "/sessions",
            Some(&json!({"resume": id}).to_string()),
        ),
    );
    let cancel = c.cancel(&id).unwrap();
    assert!(cancel.cancelled);
    lossless(
        "cancel",
        &cancel,
        &t.raw("POST", &format!("/sessions/{id}/cancel"), None),
    );

    // Refusals are typed errors.
    assert!(matches!(
        c.status("nope"),
        Err(Error::Status { status: 404, .. })
    ));
    assert!(matches!(
        t.wrong.sessions(),
        Err(Error::Status { status: 401, .. })
    ));
}

fn an_approval_is_listed_decided_and_resumed_through_the_client(t: &Target) {
    let c = &t.client;
    let id = c.create_session(&NewSession::default()).unwrap().session_id;

    let reply = c
        .prompt(&id, &PromptRequest::text("TOOL_MISSION write it"))
        .unwrap();
    assert_eq!(reply.stop_reason, "approval_required", "{reply:?}");
    let row = reply.approval.expect("the queue row");
    assert_eq!(
        (row.tool.as_str(), row.status.as_str()),
        ("write_file", "pending")
    );
    // The summary is typed, and the same on the stop body and in the list.
    let summary = row.summary.as_ref().expect("the summary of the request");
    assert_eq!(
        (summary.kind.as_str(), summary.effect.as_str()),
        ("create_file", "adds")
    );
    assert!(summary.inside_workspace, "{summary:?}");

    let list = c.approvals(Some(&id)).unwrap();
    assert_eq!(list.approvals.len(), 1);
    assert_eq!(list.approvals[0].id, row.id);
    assert_eq!(list.approvals[0].summary, row.summary);
    lossless(
        "approvals",
        &list,
        &t.raw("GET", &format!("/approvals?session={id}"), None),
    );

    let decided = c.decide(&row.id, Decision::Allow).unwrap();
    assert_eq!(
        (decided.status.as_str(), decided.resuming),
        ("approved", Some(true))
    );
    let work = t.workspace.join("approved.txt");
    common::wait_until(common::WAIT, "the approved write to land", || work.exists());
    // The opposite verdict after the fact is a conflict, never a silent flip.
    assert!(matches!(
        c.decide(&row.id, Decision::Deny),
        Err(Error::Status { status: 409, .. })
    ));
}

// A 1x1 PNG (header + IHDR; serve sniffs the type, it does not decode).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0,
    1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
];

fn status_of(e: Result<impl std::fmt::Debug, Error>) -> u16 {
    match e {
        Err(Error::Status { status, .. }) => status,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The key set of a typed answer and of the server's raw JSON must be the
/// same, for an answer whose values differ between two calls (a new id).
fn same_keys<T: Serialize>(what: &str, typed: &T, raw: &Value) {
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(
        keys(&serde_json::to_value(typed).unwrap()),
        keys(raw),
        "{what}"
    );
}

fn name_model_effort_compact_fork_and_tree_round_trip_through_the_client(t: &Target) {
    let c = &t.client;
    let id = c.create_session(&NewSession::default()).unwrap().session_id;
    c.prompt(&id, &PromptRequest::text("hello")).unwrap();
    let sess = |tail: &str| format!("/sessions/{id}/{tail}");

    // Name: the trimmed label, and the status shows it.
    let named = c
        .set_name(&id, &NameRequest::new("  Plan the work  "))
        .unwrap();
    assert_eq!(named.name, "Plan the work");
    lossless(
        "name",
        &named,
        &t.raw(
            "POST",
            &sess("name"),
            Some(&json!({"name": "Plan the work"}).to_string()),
        ),
    );
    assert_eq!(
        c.status(&id).unwrap().name.as_deref(),
        Some("Plan the work")
    );
    assert_eq!(status_of(c.set_name(&id, &NameRequest::new("  "))), 400);

    // Model: one the owner allows; anything else is a 400.
    let models = c.models().unwrap();
    let pick = models.default.clone().expect("a default model");
    let set = c.set_model(&id, &ModelRequest::new(&pick)).unwrap();
    assert_eq!(set.model, pick);
    lossless(
        "model",
        &set,
        &t.raw(
            "POST",
            &sess("model"),
            Some(&json!({"model": pick}).to_string()),
        ),
    );
    assert_eq!(c.status(&id).unwrap().model.as_deref(), Some(pick.as_str()));
    assert_eq!(
        status_of(c.set_model(&id, &ModelRequest::new("not-a-listed-model"))),
        400
    );

    // Effort: a level, then cleared with an explicit null.
    let high = c
        .set_effort(&id, &EffortRequest::new(Some("high")))
        .unwrap();
    assert_eq!(high.effort.as_deref(), Some("high"));
    lossless(
        "effort",
        &high,
        &t.raw(
            "POST",
            &sess("effort"),
            Some(&json!({"effort": "high"}).to_string()),
        ),
    );
    assert_eq!(c.status(&id).unwrap().effort.as_deref(), Some("high"));
    let cleared = c.set_effort(&id, &EffortRequest::new(None)).unwrap();
    assert_eq!(cleared.effort, None);
    lossless(
        "effort cleared",
        &cleared,
        &t.raw(
            "POST",
            &sess("effort"),
            Some(&json!({"effort": null}).to_string()),
        ),
    );
    assert_eq!(
        status_of(c.set_effort(&id, &EffortRequest::new(Some("turbo")))),
        400
    );

    // Compact: a request for the next prompt.
    let compact = c.compact(&id).unwrap();
    assert_eq!(compact.compaction, "requested");
    lossless("compact", &compact, &t.raw("POST", &sess("compact"), None));

    // Fork: a new, not live session cut at the last settled entry, which the
    // tree then shows below this one and which can be resumed.
    let fork = c.fork(&id, &ForkRequest::default()).unwrap();
    assert_eq!(fork.forked_from, id);
    assert_ne!(fork.id, id);
    assert!(fork.seq.is_some(), "{fork:?}");
    let raw_fork = t.raw("POST", &sess("fork"), Some("{}"));
    same_keys("fork", &fork, &raw_fork);
    assert_eq!(raw_fork["forkedFrom"], json!(id));
    // Cut at the same entry by its seq: the same cut point.
    let at = c.fork(&id, &ForkRequest::at(fork.seq.unwrap())).unwrap();
    assert_eq!((at.at.as_str(), at.seq), (fork.at.as_str(), fork.seq));
    assert_eq!(
        status_of(c.fork(&id, &ForkRequest::at(u64::MAX >> 12))),
        400
    );
    assert!(!c
        .sessions()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.id == fork.id));

    let tree = c.tree(&id).unwrap();
    lossless("tree", &tree, &t.raw("GET", &sess("tree"), None));
    let me = tree
        .tree
        .iter()
        .find(|n| n.current)
        .expect("the current session");
    assert_eq!(me.id, id);
    let child = tree
        .tree
        .iter()
        .find(|n| n.id == fork.id)
        .expect("the fork");
    assert_eq!(
        (child.parent.as_deref(), child.depth, child.current),
        (Some(id.as_str()), me.depth + 1, false)
    );
    assert_eq!(status_of(c.tree("nope")), 404);

    let opened = c.resume_session(&fork.id).unwrap();
    assert_eq!(opened.session_id, fork.id);
    assert!(c
        .sessions()
        .unwrap()
        .sessions
        .iter()
        .any(|s| s.id == fork.id));
}

fn an_image_is_uploaded_prompted_and_read_back_through_the_client(t: &Target) {
    let c = &t.client;
    let id = c.create_session(&NewSession::default()).unwrap().session_id;

    let up = c.upload_image(&id, PNG).unwrap();
    assert_eq!(
        (up.mime.as_str(), up.bytes),
        ("image/png", PNG.len() as u64)
    );
    assert_eq!(up.id, up.fnv1a64);
    // The same bytes are the same image: the same reference, as the server sent it.
    let again = c.upload_image(&id, PNG).unwrap();
    assert_eq!(again, up);

    // Only an image the log records can be read: the prompt records it.
    assert_eq!(status_of(c.image(&id, &up.fnv1a64)), 404);
    let reply = c
        .prompt(
            &id,
            &PromptRequest {
                text: "look".into(),
                images: vec![up.id.clone()],
            },
        )
        .unwrap();
    assert_eq!(reply.text.as_deref(), Some("ENTRIES-ANSWER"), "{reply:?}");
    let img = c.image(&id, &up.fnv1a64).unwrap();
    assert_eq!((img.mime.as_str(), img.data.as_slice()), ("image/png", PNG));

    // Refusals are typed: not an image (415), an unknown session (404).
    assert_eq!(status_of(c.upload_image(&id, b"not an image")), 415);
    assert_eq!(status_of(c.upload_image("nope", PNG)), 404);
    assert_eq!(status_of(c.image("nope", &up.fnv1a64)), 404);
}

fn a_mission_runs_to_its_budget_and_reports_through_the_client(t: &Target) {
    let c = &t.client;
    let mut req = MissionRequest::goal("tidy up");
    req.max_steps = Some(2);
    let started = c.start_mission(&req).unwrap();
    assert!(started.id.starts_with("mission-") && started.status == "running");

    let done = common::wait_for(common::WAIT, "the mission to end", || {
        let m = c.mission(&started.id).unwrap();
        (!m.running).then_some(m)
    });
    assert_eq!(done.status, "exhausted_steps", "{done:?}");
    assert!(
        done.turns.unwrap() >= 1 && done.steps.unwrap() >= 2,
        "{done:?}"
    );
    assert_eq!(done.summary.as_ref().unwrap()["goal"], "tidy up");
    lossless(
        "mission",
        &done,
        &t.raw("GET", &format!("/missions/{}", started.id), None),
    );

    // Ended: it cannot be cancelled; unknown ones and bad requests are typed.
    assert_eq!(status_of(c.cancel_mission(&started.id)), 409);
    assert_eq!(status_of(c.mission("mission-nope")), 404);
    assert_eq!(status_of(c.cancel_mission("mission-nope")), 404);
    assert_eq!(status_of(c.start_mission(&MissionRequest::goal(""))), 400);
}

mod tcp {
    use super::*;

    #[test]
    fn create_prompt_stream_and_entries_round_trip_through_the_client() {
        super::create_prompt_stream_and_entries_round_trip_through_the_client(&tcp_target());
    }

    #[test]
    fn an_approval_is_listed_decided_and_resumed_through_the_client() {
        super::an_approval_is_listed_decided_and_resumed_through_the_client(&tcp_target());
    }

    #[test]
    fn name_model_effort_compact_fork_and_tree_round_trip_through_the_client() {
        super::name_model_effort_compact_fork_and_tree_round_trip_through_the_client(&tcp_target());
    }

    #[test]
    fn an_image_is_uploaded_prompted_and_read_back_through_the_client() {
        super::an_image_is_uploaded_prompted_and_read_back_through_the_client(&tcp_target());
    }

    #[test]
    fn a_mission_runs_to_its_budget_and_reports_through_the_client() {
        super::a_mission_runs_to_its_budget_and_reports_through_the_client(&tcp_target());
    }

    /// A turn that is running is steered and then detached, and a mission that
    /// is running is cancelled; idle, both refusals are a typed 409. The
    /// provider holds each request at a gate (#812), not for a fixed time: a
    /// fixed 3 s window let the turn end by itself on a loaded runner before
    /// `detach` reached it.
    #[test]
    fn a_running_turn_is_steered_and_detached_and_a_running_mission_cancelled() {
        let (turn_gate, mission_gate) = (common::Gate::new(), common::Gate::new());
        let t = tcp_target_gated(vec![
            ("slow turn", turn_gate.clone()),
            ("slow work", mission_gate.clone()),
        ]);
        let c = &t.client;
        let id = c.create_session(&NewSession::default()).unwrap().session_id;

        assert_eq!(
            status_of(c.interrupt(&id, &InterruptRequest::text("x"))),
            409
        );
        assert_eq!(status_of(c.detach(&id)), 409);
        assert_eq!(
            status_of(c.interrupt("nope", &InterruptRequest::text("x"))),
            404
        );

        let turn = {
            let (c, id) = (c.clone(), id.clone());
            std::thread::spawn(move || c.prompt(&id, &PromptRequest::text("slow turn")))
        };
        turn_gate.wait_entered();
        assert!(c.status(&id).unwrap().busy, "the turn is at the provider");
        let steered = c
            .interrupt(&id, &InterruptRequest::text("prefer approach Y"))
            .unwrap();
        assert!(steered.queued && !steered.urgent && steered.session_id == id);
        // The same answer from the wire (a `202`, which `raw` does not take).
        let (code, raw) = (t.http)(
            "POST",
            &format!("/sessions/{id}/interrupt"),
            Some(&json!({"text": "and keep it short"}).to_string()),
        );
        assert_eq!(code, 202, "{raw}");
        lossless("interrupt", &steered, &raw);
        let detached = c.detach(&id).unwrap();
        assert!(detached.detached && detached.session_id == id);
        // The provider answers only now, after the detach was asked for.
        turn_gate.open();
        // The prompt that was waiting answers: the turn ended detached.
        let reply = turn.join().unwrap().unwrap();
        assert_eq!(reply.stop_reason, "detached", "{reply:?}");

        // A mission in flight: one at a time, and a cancel ends it `stopped`.
        let started = c.start_mission(&MissionRequest::goal("slow work")).unwrap();
        mission_gate.wait_entered();
        let running = c.mission(&started.id).unwrap();
        assert!(running.running && running.status == "running" && running.turns.is_none());
        lossless(
            "mission running",
            &running,
            &t.raw("GET", &format!("/missions/{}", started.id), None),
        );
        assert_eq!(
            status_of(c.start_mission(&MissionRequest::goal("second"))),
            409
        );
        let cancelled = c.cancel_mission(&started.id).unwrap();
        assert!(cancelled.cancelled && cancelled.id == started.id);
        mission_gate.open();
        let done = common::wait_for(common::WAIT, "the cancelled mission to end", || {
            let m = c.mission(&started.id).unwrap();
            (!m.running).then_some(m)
        });
        assert_eq!(done.status, "stopped", "{done:?}");
    }
}

#[cfg(unix)]
mod over_a_socket {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tole_client::UnixTransport;

    #[test]
    fn create_prompt_stream_and_entries_round_trip_through_the_client() {
        super::create_prompt_stream_and_entries_round_trip_through_the_client(&socket::target());
    }

    #[test]
    fn an_approval_is_listed_decided_and_resumed_through_the_client() {
        super::an_approval_is_listed_decided_and_resumed_through_the_client(&socket::target());
    }

    #[test]
    fn name_model_effort_compact_fork_and_tree_round_trip_through_the_client() {
        super::name_model_effort_compact_fork_and_tree_round_trip_through_the_client(
            &socket::target(),
        );
    }

    #[test]
    fn an_image_is_uploaded_prompted_and_read_back_through_the_client() {
        super::an_image_is_uploaded_prompted_and_read_back_through_the_client(&socket::target());
    }

    #[test]
    fn a_mission_runs_to_its_budget_and_reports_through_the_client() {
        super::a_mission_runs_to_its_budget_and_reports_through_the_client(&socket::target());
    }

    #[test]
    fn a_socket_directory_that_is_not_owner_only_is_refused_before_the_token_is_sent() {
        let serve = socket::SocketServe::spawn();
        let dir = serve.socket.parent().unwrap().to_path_buf();
        let c = Client::connect_unix(&serve.socket, Some(socket::TOKEN.into())).unwrap();
        assert_eq!(c.health().unwrap().status, "ok");

        // Serve made the directory 0700; widen it and the same client refuses,
        // and says why, without a connection.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = c.health().unwrap_err();
        assert!(matches!(err, Error::InsecureSocket { .. }), "{err:?}");
        assert!(!err.is_transient());
        let text = err.to_string();
        assert!(text.contains("0755") && text.contains("0700"), "{text}");
        assert!(!text.contains(socket::TOKEN));
        let err = Client::connect_unix(&serve.socket, Some(socket::TOKEN.into())).unwrap_err();
        assert!(matches!(err, Error::InsecureSocket { .. }), "{err:?}");
        assert!(matches!(
            UnixTransport::new(&serve.socket),
            Err(Error::InsecureSocket { .. })
        ));

        // Narrowed again, it works (the refusal was the directory, not the socket).
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(c.health().unwrap().status, "ok");
    }
}

/// Every route the client has a method for exists on the server: a rename or
/// removal there fails here. (The reverse, a server route with no client
/// method, is `client_routes.rs`.) Routes do not depend on the transport, so
/// TCP is enough.
#[test]
fn every_client_route_exists_on_the_server() {
    let serve = Serve::spawn(&spawn_mock(0));
    for route in Route::ALL {
        let path = route
            .template()
            .replace("{id}", "nope")
            .replace("{hash}", "0123456789abcdef");
        let body = (route.method() == "POST").then_some("{}");
        let (code, v) = serve.http(route.method(), &path, body);
        let route_missing = code == 404 && v["error"] == "not found";
        assert!(
            !route_missing,
            "{route:?}: the server has no `{} {}`",
            route.method(),
            route.template()
        );
    }
}
