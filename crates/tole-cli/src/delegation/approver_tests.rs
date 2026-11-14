use super::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering::SeqCst};
use std::sync::Mutex;
use tole_core::delegation::{Budget, BudgetRequest, DelegateRequest, Goal, Mode, Outcome, Spend};
use tole_core::delegation_approvals::{By, Choice, Skip, Via};
use tole_core::ledger::Lease;

const T: i64 = 1_000_000;

type Hook = Box<dyn FnMut(u32, i64) + Send>;

/// A goal `g` -> `mid` -> `kid` in a temp dir, a fake clock and a hook that
/// runs at every nap (the owner acting while the child is parked).
#[derive(Clone)]
struct Fx {
    log: PathBuf,
    kid: String,
    clock: Arc<AtomicI64>,
    naps: Arc<AtomicU32>,
    hook: Arc<Mutex<Hook>>,
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tole-dapprover-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn fx(tag: &str, goal_secs: i64) -> Fx {
    let dir = tmp(tag);
    let log = dir.join("g").join("delegation.jsonl");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut d = Delegation::open(FileStore::new(log.clone())).unwrap();
    let allow = s(&["write_file", "delegate_decide"]);
    let goal = Goal {
        id: "g",
        task: "t",
        budget: Budget {
            tokens: 1000,
            steps: 100,
            deadline: T + goal_secs,
        },
        allow: &allow,
        max_depth: 2,
        max_live: 8,
    };
    d.begin(&goal, T).unwrap();
    let lease = Lease {
        pid: 7,
        start_time: 1,
    };
    let mut spawn = |parent: &str, session: Option<&str>| {
        let r = DelegateRequest {
            profile: "p",
            task: "t",
            mode: Mode::Execute,
            budget: BudgetRequest::default(),
            profile_allow: &allow,
            workspace: None,
        };
        let h = d.delegate(parent, &r, Spend::default(), T).unwrap();
        d.start(&h.node_id, lease, session, T).unwrap();
        h.node_id
    };
    let mid = spawn("g", Some("sess-mid"));
    let kid = spawn(&mid, Some("sess-kid"));
    Fx {
        log,
        kid,
        clock: Arc::new(AtomicI64::new(T + 5)),
        naps: Arc::new(AtomicU32::new(0)),
        hook: Arc::new(Mutex::new(Box::new(|_, _| {}))),
    }
}

impl Fx {
    fn store(&self) -> Store {
        Store::new(self.log.clone())
    }

    fn on_nap(&self, f: impl FnMut(u32, i64) + Send + 'static) {
        *self.hook.lock().unwrap() = Box::new(f);
    }

    fn timing(&self) -> Timing {
        let (clock, naps, hook) = (self.clock.clone(), self.naps.clone(), self.hook.clone());
        let c2 = self.clock.clone();
        Timing {
            now: Arc::new(move || c2.load(SeqCst)),
            nap: Arc::new(move || {
                let n = naps.fetch_add(1, SeqCst) + 1;
                let now = clock.fetch_add(1, SeqCst) + 1;
                (hook.lock().unwrap())(n, now);
            }),
        }
    }

    fn env(&self, depth: &str, participant: bool) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("TOLE_AGENT_DEPTH".into(), depth.into());
        m.insert("TOLE_DELEGATION_LOG".into(), self.log.display().to_string());
        m.insert("TOLE_DELEGATION_NODE".into(), self.kid.clone());
        if participant {
            m.insert("TOLE_DISCUSSION_PARTICIPANT".into(), "1".into());
        }
        m
    }

    fn params(&self, patterns: &[&str], auto_write: bool) -> Params {
        Params {
            patterns: s(patterns),
            auto_write,
            root: self.log.parent().unwrap().to_path_buf(),
            data_dir: None,
            secrets: Arc::new(SecretSet::default()),
            cancel: CancelToken::new(),
            timing: self.timing(),
        }
    }

    fn approver(&self, secs: &str) -> DelegatedApprover {
        self.with(secs, self.params(&[], false))
    }

    fn with(&self, secs: &str, p: Params) -> DelegatedApprover {
        let src = format!("[delegation]\napprovals = \"owner\"\n{secs}\n");
        let cfg = crate::config::parse(&src, Path::new("u.toml")).unwrap();
        let env = self.env("1", false);
        let get = move |k: &str| env.get(k).cloned();
        for_child(&get, cfg.delegation.as_ref(), p).expect("a delegated child")
    }

    /// Every request as folded.
    fn entries(&self) -> Vec<tole_core::delegation_approvals::Entry> {
        self.store()
            .read(|_, a| a.state().entries().cloned().collect())
            .unwrap()
    }

    fn status(&self, now: i64) -> Status {
        let id = self.entries()[0].req.req.clone();
        self.store()
            .read(|d, a| a.state().status(&id, d, now))
            .unwrap()
            .unwrap()
    }

    /// The owner answers the one request there is.
    fn owner(&self, v: Choice, now: i64) {
        let e = self.entries().remove(0);
        let rec = Record::Decide {
            req: e.req.req.clone(),
            by: By::Owner,
            verdict: v,
            via: Via::Cli,
            fingerprint: e.req.fingerprint,
            at: now,
        };
        self.store().write(|d, a| a.append(d, rec)).unwrap();
    }
}

fn call<'a>(tool: &'a str, risk: Risk, input: &'a Value) -> ToolRequest<'a> {
    ToolRequest {
        tool,
        risk,
        input,
        description: format!("{tool} src/a.rs"),
        target: Some("src/a.rs".into()),
    }
}

fn write<'a>(input: &'a Value) -> ToolRequest<'a> {
    call("write_file", Risk::Write, input)
}

const SECS: &str = "approval_secs = 30\napproval_level_secs = 10";

#[test]
fn the_switch_picks_the_approver_only_for_a_delegated_child_when_on() {
    let f = fx("switch", 3600);
    let pick = |mode: Option<&str>, env: HashMap<String, String>| {
        let src = mode.map_or(String::new(), |m| {
            format!("[delegation]\napprovals = \"{m}\"\n")
        });
        let cfg = crate::config::parse(&src, Path::new("u.toml")).unwrap();
        let get = move |k: &str| env.get(k).cloned();
        for_child(&get, cfg.delegation.as_ref(), f.params(&[], false)).is_some()
    };
    assert!(pick(Some("owner"), f.env("1", false)));
    assert!(pick(Some("chain"), f.env("1", false)));
    assert!(
        !pick(Some("off"), f.env("1", false)),
        "off is today's stdio approver"
    );
    assert!(!pick(None, f.env("1", false)), "unset is off");
    assert!(!pick(Some("owner"), f.env("0", false)), "not a child");
    assert!(!pick(Some("owner"), HashMap::new()), "not a child");
    let mut no_node = f.env("1", false);
    no_node.remove("TOLE_DELEGATION_NODE");
    assert!(!pick(Some("owner"), no_node), "a classic agent child");
    assert!(
        !pick(Some("owner"), f.env("1", true)),
        "a reviewer never asks for a Write"
    );
}

#[test]
fn the_allow_list_decides_first_and_nothing_is_asked() {
    let f = fx("allow", 3600);
    let i = json!({"path": "src/a.rs"});
    let a = f.with(SECS, f.params(&["write_file", "!edit_file"], false));
    assert_eq!(a.decide(&write(&i)), Verdict::Allow);
    assert_eq!(a.decide(&call("edit_file", Risk::Write, &i)), Verdict::Deny);
    assert_eq!(
        a.decide(&call("read_file", Risk::ReadOnly, &i)),
        Verdict::Allow
    );
    // A deny pattern beats a ReadOnly call too, as in the stdio approver.
    let d = f.with(SECS, f.params(&["!read_file"], false));
    assert_eq!(
        d.decide(&call("read_file", Risk::ReadOnly, &i)),
        Verdict::Deny
    );
    // Destructive is never asked and never allowed.
    let all = f.with(SECS, f.params(&["*"], true));
    assert_eq!(
        all.decide(&call("delete_file", Risk::Destructive, &i)),
        Verdict::Deny
    );
    // --yes allows a Write.
    let yes = f.with(SECS, f.params(&[], true));
    assert_eq!(yes.decide(&write(&i)), Verdict::Allow);
    assert_eq!(f.naps.load(SeqCst), 0);
    assert!(!approvals_path(&f.log).exists(), "nothing was written");
}

#[test]
fn it_is_not_interactive_so_destructive_tools_are_not_registered() {
    let f = fx("interactive", 3600);
    assert!(!f.approver(SECS).interactive());
}

#[test]
fn an_owner_allow_runs_the_call_once_and_the_next_one_asks_again() {
    let f = fx("allowed", 3600);
    let store = f.store();
    f.on_nap({
        let f2 = f.clone();
        move |n, now| {
            if n == 2 {
                f2.owner(Choice::Allow, now);
            }
        }
    });
    let a = f.approver(SECS);
    let i = json!({"path": "src/a.rs", "content": "x"});
    assert_eq!(a.decide(&write(&i)), Verdict::Allow);
    assert_eq!(f.naps.load(SeqCst), 2, "parked until the answer");
    // The consume is on disk before Allow returned, with the child's own
    // fingerprint of the call.
    let e = f.entries().remove(0);
    let session = "sess-kid";
    let root = f.log.parent().unwrap().to_str().unwrap();
    let fp = fingerprint(&f.kid, session, "write_file", &i, Some("src/a.rs"), root);
    assert_eq!(
        (e.req.fingerprint.clone(), e.consumed_at.is_some()),
        (fp, true)
    );
    assert_eq!(f.status(f.clock.load(SeqCst)), Status::Consumed);
    // One-shot: the same call asks again (a new request) and, unanswered,
    // is denied.
    f.on_nap(|_, _| {});
    assert_eq!(a.decide(&write(&i)), Verdict::Deny);
    assert_eq!(store.read(|_, a| a.state().entries().count()).unwrap(), 2);
}

#[test]
fn an_owner_deny_is_a_deny_and_nothing_is_consumed() {
    let f = fx("denied", 3600);
    f.on_nap({
        let f2 = f.clone();
        move |_, now| f2.owner(Choice::Deny, now)
    });
    let i = json!({"path": "src/a.rs"});
    assert_eq!(f.approver(SECS).decide(&write(&i)), Verdict::Deny);
    let e = f.entries().remove(0);
    assert!(e.consumed_at.is_none() && e.decision.is_some());
}

#[test]
fn no_answer_is_denied_at_the_window_and_the_expiry_is_recorded() {
    let f = fx("timeout", 3600);
    let i = json!({"path": "src/a.rs"});
    assert_eq!(f.approver(SECS).decide(&write(&i)), Verdict::Deny);
    let e = f.entries().remove(0);
    assert_eq!((e.req.expires_at, e.req.level_secs), (T + 5 + 30, 10));
    // The wait ended at the window, not before and not long after.
    assert_eq!(f.clock.load(SeqCst), e.req.expires_at);
    assert_eq!(e.expired.map(|x| x.0), Some(ExpireReason::Timeout));
}

#[test]
fn the_wait_never_outlasts_the_node_deadline() {
    // The goal (and so the child) must settle soon, while the config asks
    // for an hour.
    let f = fx("deadline", 25);
    let deadline = Delegation::open(FileStore::new(f.log.clone()))
        .unwrap()
        .poll(&f.kid)
        .unwrap()
        .budget
        .deadline;
    assert!(deadline > T + 5 && deadline <= T + 25, "{deadline}");
    let i = json!({"path": "src/a.rs"});
    let a = f.with("approval_secs = 3600", f.params(&[], false));
    assert_eq!(a.decide(&write(&i)), Verdict::Deny);
    let e = f.entries().remove(0);
    assert_eq!(e.req.expires_at, deadline);
    assert!(f.clock.load(SeqCst) <= deadline);
}

#[test]
fn a_cancelled_session_stops_waiting_and_records_no_decision() {
    let f = fx("cancel", 3600);
    let p = f.params(&[], false);
    let cancel = p.cancel.clone();
    f.on_nap(move |n, _| {
        if n == 3 {
            cancel.cancel();
        }
    });
    let i = json!({"path": "src/a.rs"});
    assert_eq!(f.with(SECS, p).decide(&write(&i)), Verdict::Deny);
    assert_eq!(f.naps.load(SeqCst), 3);
    let e = f.entries().remove(0);
    assert!(e.decision.is_none() && e.consumed_at.is_none() && e.expired.is_none());
}

#[test]
fn an_allow_after_the_window_or_for_a_settled_node_is_no_allow() {
    // Answered, but the clock is past the window by the time the child looks.
    let f = fx("late", 3600);
    f.on_nap({
        let f2 = f.clone();
        move |_, now| {
            f2.owner(Choice::Allow, now);
            f2.clock.store(T + 5 + 30, SeqCst);
        }
    });
    let i = json!({"path": "src/a.rs"});
    assert_eq!(f.approver(SECS).decide(&write(&i)), Verdict::Deny);
    assert!(f.entries()[0].consumed_at.is_none());
    // The node settles between the allow and the consume.
    let f = fx("settled", 3600);
    let (log, kid) = (f.log.clone(), f.kid.clone());
    f.on_nap({
        let f2 = f.clone();
        move |_, now| {
            f2.owner(Choice::Allow, now);
            let mut d = Delegation::open(FileStore::new(log.clone())).unwrap();
            let out = Outcome::Failed;
            d.settle(&kid, out, Spend::default(), None, None, now)
                .unwrap();
        }
    });
    assert_eq!(f.approver(SECS).decide(&write(&i)), Verdict::Deny);
    assert!(f.entries()[0].consumed_at.is_none());
}

#[test]
fn a_second_open_request_of_the_node_is_denied_without_waiting() {
    let f = fx("pending", 3600);
    let i = json!({"path": "src/a.rs"});
    // The first call parks and is cancelled, leaving its request pending.
    let p = f.params(&[], false);
    let cancel = p.cancel.clone();
    f.on_nap(move |_, _| cancel.cancel());
    assert_eq!(f.with(SECS, p).decide(&write(&i)), Verdict::Deny);
    let before = f.naps.load(SeqCst);
    f.on_nap(|_, _| panic!("a refused request must not wait"));
    assert_eq!(f.approver(SECS).decide(&write(&i)), Verdict::Deny);
    assert_eq!(f.naps.load(SeqCst), before);
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn a_node_without_a_session_cannot_ask() {
    let f = fx("nosession", 3600);
    // A node the log never started has no session to bind a request to.
    let mut d = Delegation::open(FileStore::new(f.log.clone())).unwrap();
    let r = DelegateRequest {
        profile: "p",
        task: "t",
        mode: Mode::Execute,
        budget: BudgetRequest::default(),
        profile_allow: &[],
        workspace: None,
    };
    let node = d.delegate("g", &r, Spend::default(), T).unwrap().node_id;
    let mut env = f.env("1", false);
    env.insert("TOLE_DELEGATION_NODE".into(), node);
    let cfg =
        crate::config::parse("[delegation]\napprovals = \"owner\"\n", Path::new("u.toml")).unwrap();
    let get = move |k: &str| env.get(k).cloned();
    let a = for_child(&get, cfg.delegation.as_ref(), f.params(&[], false)).unwrap();
    let i = json!({"path": "src/a.rs"});
    assert_eq!(a.decide(&write(&i)), Verdict::Deny);
    assert_eq!(f.naps.load(SeqCst), 0);
}

#[test]
fn the_stored_copy_is_redacted_and_a_secret_makes_the_request_owner_only() {
    let f = fx("secret", 3600);
    let p = Params {
        secrets: Arc::new(SecretSet::default().with("DEPLOY_KEY", "s3cr3t-value-123456")),
        ..f.params(&[], false)
    };
    let i = json!({"path": "src/a.rs", "content": "key=s3cr3t-value-123456"});
    let mut r = write(&i);
    r.description = "write src/a.rs with key=s3cr3t-value-123456".into();
    // The owner allows; the consume must carry the fingerprint of the RAW call.
    f.on_nap({
        let f2 = f.clone();
        move |_, now| f2.owner(Choice::Allow, now)
    });
    assert_eq!(f.with(SECS, p).decide(&r), Verdict::Allow);
    let r = f.entries().remove(0).req;
    assert!(!r.input.to_string().contains("s3cr3t") && !r.display.contains("s3cr3t"));
    assert!(r.secrets);
    assert!(r.chain.iter().all(|l| l.skipped == Some(Skip::OwnerOnly)));
    let root = f.log.parent().unwrap().to_str().unwrap();
    let fp = fingerprint(&f.kid, "sess-kid", "write_file", &i, Some("src/a.rs"), root);
    assert_eq!(r.fingerprint, fp);
}

#[test]
fn only_known_builtin_tools_are_readable_by_a_level() {
    let f = fx("origin", 3600);
    let i = json!({"path": "src/a.rs"});
    let a = f.approver(SECS);
    assert_eq!(a.decide(&write(&i)), Verdict::Deny);
    let first = f.entries().remove(0).req;
    assert!(
        first.chain.iter().all(|l| l.skipped.is_none()),
        "{:?}",
        first.chain
    );
    // A different tool of the same node, once the first is over.
    let ext = call("mcp_deploy", Risk::Write, &i);
    assert_eq!(a.decide(&ext), Verdict::Deny);
    let e = f.entries().remove(1).req;
    assert_ne!(e.origin, Origin::Builtin);
    assert!(e.chain.iter().all(|l| l.skipped == Some(Skip::OwnerOnly)));
}

#[test]
fn a_target_in_the_data_dir_or_a_tole_dir_is_protected() {
    let root = tmp("protect");
    let data = tmp("protect-data");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join(".tole")).unwrap();
    let p = |t: Option<&str>, d: Option<&Path>| is_protected(&root, t, d);
    assert!(!p(Some("src/a.rs"), Some(&data)));
    assert!(!p(None, Some(&data)));
    assert!(p(Some(".tole/config.toml"), None));
    // A root inside the data dir: everything under it is protected.
    assert!(is_protected(
        &data,
        Some("delegation/x/approvals.jsonl"),
        Some(&data)
    ));
    // A not-yet-existing file resolves through its existing parent.
    assert!(p(Some(".tole/new/dir/file"), None));
    let bare = tmp("protect-bare");
    assert!(is_protected(&bare, Some(".tole/config.toml"), None));
    assert!(!is_protected(&bare, Some("src/new/dir/file"), None));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&data, root.join("src/link")).unwrap();
        assert!(p(Some("src/link/approvals.jsonl"), Some(&data)));
    }
}

#[cfg(unix)]
#[test]
fn the_approvals_file_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx("mode", 3600);
    let i = json!({"path": "src/a.rs"});
    f.approver(SECS).decide(&write(&i));
    let mode = std::fs::metadata(approvals_path(&f.log))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn the_log_sits_next_to_the_delegation_log() {
    assert_eq!(
        approvals_path(Path::new("/d/delegation/abc/delegation.jsonl")),
        Path::new("/d/delegation/abc/approvals.jsonl")
    );
}

#[test]
fn the_defaults_are_900_and_120_seconds() {
    let f = fx("defaults", 7200);
    let i = json!({"path": "src/a.rs"});
    assert_eq!(
        f.with("", f.params(&[], false)).decide(&write(&i)),
        Verdict::Deny
    );
    let e = f.entries().remove(0);
    assert_eq!((e.req.expires_at, e.req.level_secs), (T + 5 + 900, 120));
}

#[test]
fn a_consume_needs_the_childs_own_fingerprint_and_works_once() {
    let f = fx("consume", 3600);
    // The owner allows at the first nap and the session is cancelled at the
    // same moment, so the child stops before it consumes.
    let p = f.params(&[], false);
    let cancel = p.cancel.clone();
    f.on_nap({
        let f2 = f.clone();
        move |_, now| {
            f2.owner(Choice::Allow, now);
            cancel.cancel();
        }
    });
    let a = f.with(SECS, p);
    let i = json!({"path": "src/a.rs"});
    assert_eq!(a.decide(&write(&i)), Verdict::Deny);
    let id = f.entries()[0].req.req.clone();
    let fp = f.entries()[0].req.fingerprint.clone();
    let now = f.clock.load(SeqCst);
    assert_eq!(a.consume(&id, &"0".repeat(64), now), Verdict::Deny);
    assert!(f.entries()[0].consumed_at.is_none());
    assert_eq!(a.consume(&id, &fp, now), Verdict::Allow);
    assert!(f.entries()[0].consumed_at.is_some());
    assert_eq!(a.consume(&id, &fp, now), Verdict::Deny, "once");
}
