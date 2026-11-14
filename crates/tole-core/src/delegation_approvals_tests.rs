use super::*;
use crate::delegation::{
    Budget, BudgetRequest, DelegateRequest, Delegation, Goal, Mode, Outcome, Spend,
};
use crate::ledger::{Lease, LedgerStore, MemoryStore};
use serde_json::json;

const T: i64 = 1_000_000;
const L: Lease = Lease {
    pid: 7,
    start_time: 1,
};

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Goal `g` (root, level 2) -> `mid` (level 1) -> `kid` (the requester).
struct Fx {
    d: Delegation<MemoryStore>,
    store: MemoryStore,
    a: Approvals<MemoryStore>,
    mid: String,
    kid: String,
}

fn fx(root: &[&str], mid: &[&str]) -> Fx {
    let mut d = Delegation::open(MemoryStore::default()).unwrap();
    let allow = s(root);
    let budget = Budget {
        tokens: 1000,
        steps: 100,
        deadline: T + 7200,
    };
    let goal = Goal {
        id: "g",
        task: "t",
        budget,
        allow: &allow,
        max_depth: 2,
        max_live: 8,
    };
    d.begin(&goal, T).unwrap();
    let mut spawn = |parent: &str, profile: &[String], session: &str| {
        let r = DelegateRequest {
            profile: "p",
            task: "t",
            mode: Mode::Execute,
            budget: BudgetRequest::default(),
            profile_allow: profile,
            workspace: None,
        };
        let h = d.delegate(parent, &r, Spend::default(), T).unwrap();
        d.start(&h.node_id, L, Some(session), T).unwrap();
        h.node_id
    };
    let mid = spawn("g", &s(mid), "sess-mid");
    let kid = spawn(&mid, &[], "sess-kid");
    let store = MemoryStore::default();
    let a = Approvals::open(store.clone(), &d).unwrap();
    Fx {
        d,
        store,
        a,
        mid,
        kid,
    }
}

fn std_fx() -> Fx {
    fx(
        &["write_file", "delegate_decide"],
        &["write_file", "delegate_decide"],
    )
}

fn input() -> Value {
    json!({"path": "src/a.rs", "content": "x"})
}

/// A well-formed request of `kid` for `write_file src/a.rs`; `f` may break it.
fn req_at(fx: &Fx, nonce: &str, now: i64, f: impl FnOnce(&mut Request)) -> Request {
    let node = fx.d.poll(&fx.kid).unwrap();
    let mut r = Request {
        req: format!("dap-{nonce}"),
        goal: "g".into(),
        node: fx.kid.clone(),
        session: "sess-kid".into(),
        tool: "write_file".into(),
        risk: Risk::Write,
        origin: Origin::Builtin,
        target: Some("src/a.rs".into()),
        input: input(),
        display: "write_file src/a.rs".into(),
        truncated: false,
        secrets: false,
        protected: false,
        fingerprint: String::new(),
        workspace: "/w".into(),
        chain: vec![],
        level_secs: 120,
        expires_at: (now + 900).min(node.budget.deadline),
        at: now,
    };
    f(&mut r);
    if r.fingerprint.is_empty() {
        let (n, se, t) = (&r.node, &r.session, &r.tool);
        r.fingerprint = fingerprint(n, se, t, &input(), r.target.as_deref(), &r.workspace);
    }
    if r.chain.is_empty() {
        r.chain = build_chain(&fx.d, node, &r).unwrap();
    }
    r
}

fn id(nonce: &str) -> String {
    format!("dap-{nonce}")
}

fn ask(fx: &mut Fx, nonce: &str, now: i64) -> Result<(), ApprovalError> {
    let r = req_at(fx, nonce, now, |_| {});
    fx.a.append(&fx.d, Record::Request(r))
}

fn fp(fx: &Fx, req: &str) -> String {
    fx.a.state().entry(req).unwrap().req.fingerprint.clone()
}

fn decide_rec(fx: &Fx, req: &str, by: By, v: Choice, via: Via, at: i64) -> Record {
    Record::Decide {
        req: req.into(),
        by,
        verdict: v,
        via,
        fingerprint: fp(fx, req),
        at,
    }
}

fn decide(fx: &mut Fx, req: &str, by: By, v: Choice, at: i64) -> Result<(), ApprovalError> {
    let via = if by == By::Owner { Via::Cli } else { Via::Tool };
    let rec = decide_rec(fx, req, by, v, via, at);
    fx.a.append(&fx.d, rec)
}

fn decide_mid(fx: &mut Fx, req: &str, v: Choice, at: i64) -> Result<(), ApprovalError> {
    let by = mid(fx);
    decide(fx, req, by, v, at)
}

fn consume(fx: &mut Fx, req: &str, node: &str, at: i64) -> Result<(), ApprovalError> {
    let rec = Record::Consume {
        req: req.into(),
        node: node.into(),
        fingerprint: fp(fx, req),
        at,
    };
    fx.a.append(&fx.d, rec)
}

/// Writes a line with no validation (a forger with the file open).
fn forge(fx: &mut Fx, rec: &Record) {
    fx.store
        .append(format!("{}\n", rec.to_line()).as_bytes())
        .unwrap();
    fx.a.refresh(&fx.d).unwrap();
}

fn mid(fx: &Fx) -> By {
    By::Agent {
        node: fx.mid.clone(),
        level: 1,
    }
}

fn root() -> By {
    By::Agent {
        node: "g".into(),
        level: 2,
    }
}

fn status(fx: &Fx, req: &str, now: i64) -> Status {
    fx.a.state().status(req, &fx.d, now).unwrap()
}

fn refusal(r: Result<(), ApprovalError>) -> Refusal {
    match r.unwrap_err() {
        ApprovalError::Refused(x) => x,
        e => panic!("not a refusal: {e}"),
    }
}

fn skips(r: &Request) -> Vec<Option<Skip>> {
    r.chain.iter().map(|l| l.skipped).collect()
}

#[test]
fn fingerprint_binds_every_field_and_ignores_key_order() {
    let i = json!({"a": 1, "b": {"y": 1, "z": 2}});
    let base = fingerprint("n", "s", "t", &i, Some("p"), "/w");
    assert!(is_hex64(&base));
    let reordered = json!({"b": {"z": 2, "y": 1}, "a": 1});
    assert_eq!(
        base,
        fingerprint("n", "s", "t", &reordered, Some("p"), "/w")
    );
    let other = json!({"a": 2, "b": {"y": 1, "z": 2}});
    for (name, f) in [
        ("node", fingerprint("m", "s", "t", &i, Some("p"), "/w")),
        ("session", fingerprint("n", "x", "t", &i, Some("p"), "/w")),
        ("tool", fingerprint("n", "s", "u", &i, Some("p"), "/w")),
        ("input", fingerprint("n", "s", "t", &other, Some("p"), "/w")),
        ("target", fingerprint("n", "s", "t", &i, Some("q"), "/w")),
        ("no target", fingerprint("n", "s", "t", &i, None, "/w")),
        ("workspace", fingerprint("n", "s", "t", &i, Some("p"), "/x")),
    ] {
        assert_ne!(base, f, "{name} must change the fingerprint");
    }
    // A field boundary cannot be shifted.
    let a = fingerprint("ab", "c", "t", &i, None, "/w");
    assert_ne!(a, fingerprint("a", "bc", "t", &i, None, "/w"));
}

#[test]
fn records_parse_strictly() {
    let ok = Record::Consume {
        req: "dap-1".into(),
        node: "n".into(),
        fingerprint: "f".into(),
        at: 1,
    };
    assert_eq!(Record::parse(&ok.to_line()).unwrap(), Some(ok.clone()));
    let mut v: Value = serde_json::from_str(&ok.to_line()).unwrap();
    v["extra"] = json!(1);
    let e = Record::parse(&v.to_string()).unwrap_err();
    assert!(matches!(e, LedgerError::Corrupt { .. }), "{e}");
    let mut v: Value = serde_json::from_str(&ok.to_line()).unwrap();
    v["kind"] = json!("grant");
    assert_eq!(Record::parse(&v.to_string()).unwrap(), None);
    let mut v: Value = serde_json::from_str(&ok.to_line()).unwrap();
    v["v"] = json!(APPROVALS_VERSION + 1);
    let e = Record::parse(&v.to_string()).unwrap_err();
    assert!(matches!(e, LedgerError::NewerVersion { .. }), "{e}");
    // Unknown fields are refused inside a request too.
    let f = std_fx();
    let mut v: Value = serde_json::to_value(Record::Request(req_at(&f, "a", T, |_| {}))).unwrap();
    v["note"] = json!("allow me");
    assert!(Record::parse(&v.to_string()).is_err());
}

#[test]
fn an_unknown_field_on_a_decision_is_not_a_decision() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let rec = decide_rec(&f, &id("a"), mid(&f), Choice::Allow, Via::Tool, T + 1);
    let mut v: Value = serde_json::from_str(&rec.to_line()).unwrap();
    v["by_owner"] = json!(true);
    f.store.append(format!("{v}\n").as_bytes()).unwrap();
    f.a.refresh(&f.d).unwrap();
    assert!(f.a.state().entry(&id("a")).unwrap().decision.is_none());
    assert_eq!(f.a.state().rejected, 1);
}

#[test]
fn the_chain_is_read_from_the_tree_and_recorded() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let r = &f.a.state().entry(&id("a")).unwrap().req;
    let want = [(f.mid.clone(), 1), ("g".to_string(), 0)];
    for (l, (node, depth)) in r.chain.iter().zip(want) {
        assert_eq!((l.node.clone(), l.depth, l.skipped), (node, depth, None));
    }
    assert_eq!(r.chain.len(), 2);
}

#[test]
fn levels_that_cannot_decide_are_skipped_with_a_reason() {
    let call = |f: &Fx| skips(&req_at(f, "a", T, |_| {}));
    // No delegate_decide grant at the middle level.
    let f = fx(&["write_file", "delegate_decide"], &["write_file"]);
    assert_eq!(call(&f), [Some(Skip::NoDecideTool), None]);
    // A grant but no write_file of its own: outside its authority.
    let f = fx(&["write_file", "delegate_decide"], &["delegate_decide"]);
    assert_eq!(call(&f), [Some(Skip::OutsideAuthority), None]);
    // A deny of the root is inherited by every level: nobody can allow.
    let f = fx(
        &["write_file", "delegate_decide", "!write_file"],
        &["write_file", "delegate_decide"],
    );
    assert_eq!(call(&f), [Some(Skip::OutsideAuthority); 2]);
    // An always-owner request skips every agent level.
    let f = std_fx();
    let r = req_at(&f, "a", T, |r| r.secrets = true);
    assert_eq!(skips(&r), [Some(Skip::OwnerOnly); 2]);
}

#[test]
fn the_always_owner_table() {
    let f = std_fx();
    let class = |mutate: fn(&mut Request)| owner_only_class(&req_at(&f, "a", T, mutate));
    assert_eq!(class(|_| {}), None);
    type M = fn(&mut Request);
    let table: [(&str, M, OwnerOnly); 16] = [
        (
            "secret-bound tool",
            |r| r.secrets = true,
            OwnerOnly::Secrets,
        ),
        (
            "protected target",
            |r| r.protected = true,
            OwnerOnly::Protected,
        ),
        (
            "cron tool",
            |r| r.tool = "cron_add".into(),
            OwnerOnly::Protected,
        ),
        (
            "trust tool",
            |r| r.tool = "config_trust".into(),
            OwnerOnly::Protected,
        ),
        (
            "approve tool",
            |r| r.tool = "plugin_approve".into(),
            OwnerOnly::Protected,
        ),
        (
            "mcp tool",
            |r| r.origin = Origin::Mcp,
            OwnerOnly::ExternalEffect,
        ),
        (
            "plugin tool",
            |r| r.origin = Origin::Plugin,
            OwnerOnly::ExternalEffect,
        ),
        (
            "skill tool",
            |r| r.origin = Origin::Skill,
            OwnerOnly::ExternalEffect,
        ),
        ("truncated", |r| r.truncated = true, OwnerOnly::Oversized),
        (
            "no target",
            |r| r.target = None,
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "dot dot",
            |r| r.target = Some("../x".into()),
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "absolute",
            |r| r.target = Some("/etc/passwd".into()),
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "git hook",
            |r| r.target = Some(".git/hooks/pre-commit".into()),
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "build file",
            |r| r.target = Some("Makefile".into()),
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "ci file",
            |r| r.target = Some(".github/workflows/ci.yml".into()),
            OwnerOnly::OutsideWorkspace,
        ),
        (
            "script",
            |r| r.target = Some("run.sh".into()),
            OwnerOnly::OutsideWorkspace,
        ),
    ];
    for (name, m, want) in table {
        assert_eq!(class(m), Some(want), "{name}");
    }
    // A command has no path target.
    let cmd = |r: &mut Request| {
        r.tool = "run_command".into();
        r.target = None;
    };
    assert_eq!(class(cmd), None);
}

#[test]
fn a_level_decides_only_within_its_own_list() {
    let f = std_fx();
    let r = req_at(&f, "a", T, |_| {});
    let yes = |allow: &[&str], r: &Request| level_may_decide(&s(allow), r);
    assert!(yes(&["write_file"], &r));
    assert!(yes(&["write_*"], &r));
    assert!(yes(&["write_file@workspace"], &r));
    assert!(!yes(&[], &r));
    assert!(!yes(&["edit_file"], &r));
    assert!(!yes(&["write_file", "!write_file"], &r), "deny wins");
    assert!(!yes(&["!write_file", "write_file"], &r), "deny wins");
    // The workspace guard holds even under a bare allow.
    let hook = req_at(&f, "b", T, |r| r.target = Some(".git/hooks/x".into()));
    assert!(!yes(&["write_file"], &hook));
    let none = req_at(&f, "c", T, |r| r.target = None);
    assert!(!yes(&["write_file"], &none));
    // Only a Write can be decided by a level.
    let d = req_at(&f, "d", T, |r| r.risk = Risk::Destructive);
    assert!(!yes(&["write_file"], &d));
    // A command has no path: a bare tool allow is not enough.
    let cmd = req_at(&f, "e", T, |r| {
        r.tool = "run_command".into();
        r.target = None;
        r.input = json!({"command": "cargo test"});
    });
    assert!(!yes(&["run_command"], &cmd));
    #[cfg(feature = "shell-tools")]
    {
        assert!(yes(&["run_command:cargo test"], &cmd));
        assert!(!yes(&["run_command:git status"], &cmd));
    }
}

#[test]
fn the_first_valid_decision_wins_in_both_orders() {
    for (first, second) in [(Choice::Allow, Choice::Deny), (Choice::Deny, Choice::Allow)] {
        let mut f = std_fx();
        ask(&mut f, "a", T).unwrap();
        decide_mid(&mut f, &id("a"), first, T + 1).unwrap();
        // A second valid decision (the owner may always decide) loses.
        let e = decide(&mut f, &id("a"), By::Owner, second, T + 2);
        assert_eq!(refusal(e), Refusal::RaceLost);
        let late = decide_rec(&f, &id("a"), By::Owner, second, Via::Serve, T + 3);
        forge(&mut f, &late);
        let e = f.a.state().entry(&id("a")).unwrap();
        assert_eq!(e.decision.as_ref().unwrap().verdict, first);
        assert_eq!(e.race_lost, 1);
    }
    // The owner first, then the level.
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide(&mut f, &id("a"), By::Owner, Choice::Deny, T + 1).unwrap();
    let e = decide_mid(&mut f, &id("a"), Choice::Allow, T + 2);
    assert_eq!(refusal(e), Refusal::RaceLost);
    assert_eq!(status(&f, &id("a"), T + 3), Status::Denied);
}

#[test]
fn only_the_current_level_decides() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let e = decide(&mut f, &id("a"), root(), Choice::Allow, T + 1);
    assert_eq!(
        refusal(e),
        Refusal::NotCurrentLevel,
        "grandparent too early"
    );
    let wrong_level = By::Agent {
        node: f.mid.clone(),
        level: 2,
    };
    let e = decide(&mut f, &id("a"), wrong_level, Choice::Allow, T + 1);
    assert_eq!(refusal(e), Refusal::NotCurrentLevel);
    let own = By::Agent {
        node: f.kid.clone(),
        level: 1,
    };
    let e = decide(&mut f, &id("a"), own, Choice::Allow, T + 1);
    assert_eq!(
        refusal(e),
        Refusal::NotCurrentLevel,
        "never its own request"
    );
    // After the window the parent is no longer current; the root is.
    let e = decide_mid(&mut f, &id("a"), Choice::Allow, T + 120);
    assert_eq!(refusal(e), Refusal::NotCurrentLevel);
    decide(&mut f, &id("a"), root(), Choice::Allow, T + 120).unwrap();
    assert_eq!(status(&f, &id("a"), T + 121), Status::Allowed);
}

#[test]
fn a_skipped_level_never_decides_and_costs_no_time() {
    // The middle level has no grant: the root is current from the start.
    let mut f = fx(&["write_file", "delegate_decide"], &["write_file"]);
    ask(&mut f, "a", T).unwrap();
    assert_eq!(status(&f, &id("a"), T), Status::Pending { level: Some(1) });
    let e = decide_mid(&mut f, &id("a"), Choice::Allow, T + 1);
    assert_eq!(refusal(e), Refusal::NotCurrentLevel);
    assert_eq!(
        status(&f, &id("a"), T + 119),
        Status::Pending { level: Some(1) }
    );
    assert_eq!(
        status(&f, &id("a"), T + 120),
        Status::Pending { level: None }
    );
    // An always-owner request is the owner's from the start.
    let mut f = std_fx();
    let r = req_at(&f, "b", T, |r| r.secrets = true);
    f.a.append(&f.d, Record::Request(r)).unwrap();
    assert_eq!(status(&f, &id("b"), T), Status::Pending { level: None });
    let e = decide_mid(&mut f, &id("b"), Choice::Allow, T + 1);
    assert_eq!(refusal(e), Refusal::NotCurrentLevel);
    decide(&mut f, &id("b"), By::Owner, Choice::Allow, T + 1).unwrap();
}

#[test]
fn the_owner_decides_through_an_owner_surface_only() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let r = f.a.append(
        &f.d,
        decide_rec(&f, &id("a"), By::Owner, Choice::Allow, Via::Tool, T + 1),
    );
    assert_eq!(refusal(r), Refusal::BadVia);
    let r = f.a.append(
        &f.d,
        decide_rec(&f, &id("a"), mid(&f), Choice::Allow, Via::Cli, T + 1),
    );
    assert_eq!(refusal(r), Refusal::BadVia);
    // Any time while pending, even before the parent's window ends.
    decide(&mut f, &id("a"), By::Owner, Choice::Allow, T + 1).unwrap();
}

#[test]
fn a_decision_is_bound_to_the_request_fingerprint() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let mut rec = decide_rec(&f, &id("a"), mid(&f), Choice::Allow, Via::Tool, T + 1);
    if let Record::Decide { fingerprint, .. } = &mut rec {
        *fingerprint = "0".repeat(64);
    }
    assert_eq!(refusal(f.a.append(&f.d, rec.clone())), Refusal::Fingerprint);
    forge(&mut f, &rec);
    assert!(f.a.state().entry(&id("a")).unwrap().decision.is_none());
}

#[test]
fn the_window_moves_up_on_the_clock() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let at = |t| status(&f, &id("a"), T + t);
    assert_eq!(at(0), Status::Pending { level: Some(0) });
    assert_eq!(at(119), Status::Pending { level: Some(0) });
    assert_eq!(at(120), Status::Pending { level: Some(1) });
    assert_eq!(at(239), Status::Pending { level: Some(1) });
    assert_eq!(at(240), Status::Pending { level: None });
    assert_eq!(at(899), Status::Pending { level: None });
    assert_eq!(at(900), Status::Expired(ExpireReason::Timeout));
}

#[test]
fn an_explicit_escalation_moves_up_and_restarts_the_window() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let esc = |from: &str, to: Option<&str>, reason, at| Record::Escalate {
        req: id("a"),
        from: from.into(),
        to: to.map(Into::into),
        reason,
        at,
    };
    let ex = EscalateReason::Explicit;
    // Not the current level, or not the next level: refused.
    let bad = esc("g", None, ex, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::BadEscalation);
    let bad = esc(&f.mid, None, ex, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::BadEscalation);
    let bad = esc("nobody", Some("g"), ex, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::BadEscalation);
    // Right level, wrong target: `to` must be the next level.
    let bad = esc("g", Some("g"), ex, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::BadEscalation);
    // A note must come from a level of the chain too.
    let bad = esc("nobody", None, EscalateReason::Timeout, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::BadEscalation);
    // A clock note does not move anything.
    let note = esc(&f.mid, Some("g"), EscalateReason::Timeout, T + 5);
    f.a.append(&f.d, note).unwrap();
    assert_eq!(
        status(&f, &id("a"), T + 6),
        Status::Pending { level: Some(0) }
    );
    let up = esc(&f.mid, Some("g"), ex, T + 10);
    f.a.append(&f.d, up).unwrap();
    assert_eq!(
        status(&f, &id("a"), T + 11),
        Status::Pending { level: Some(1) }
    );
    // The root's window runs from the escalation, not the request.
    assert_eq!(
        status(&f, &id("a"), T + 129),
        Status::Pending { level: Some(1) }
    );
    assert_eq!(
        status(&f, &id("a"), T + 130),
        Status::Pending { level: None }
    );
    let e = decide_mid(&mut f, &id("a"), Choice::Allow, T + 11);
    assert_eq!(refusal(e), Refusal::NotCurrentLevel);
    // Nothing escalates after a decision.
    decide(&mut f, &id("a"), root(), Choice::Deny, T + 11).unwrap();
    let late = esc("g", None, ex, T + 12);
    assert_eq!(refusal(f.a.append(&f.d, late)), Refusal::Settled);
}

#[test]
fn an_allow_is_consumed_once_by_the_requester() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let kid = f.kid.clone();
    assert_eq!(
        refusal(consume(&mut f, &id("a"), &kid, T + 1)),
        Refusal::NotAllowed
    );
    decide_mid(&mut f, &id("a"), Choice::Allow, T + 2).unwrap();
    let mid_id = f.mid.clone();
    let e = consume(&mut f, &id("a"), &mid_id, T + 3);
    assert_eq!(refusal(e), Refusal::NotRequester);
    let bad = Record::Consume {
        req: id("a"),
        node: kid.clone(),
        fingerprint: "1".repeat(64),
        at: T + 3,
    };
    assert_eq!(refusal(f.a.append(&f.d, bad)), Refusal::Fingerprint);
    consume(&mut f, &id("a"), &kid, T + 3).unwrap();
    assert_eq!(status(&f, &id("a"), T + 4), Status::Consumed);
    // Double consume, also written by a forger straight into the file.
    assert_eq!(
        refusal(consume(&mut f, &id("a"), &kid, T + 4)),
        Refusal::AlreadyConsumed
    );
    let again = Record::Consume {
        req: id("a"),
        node: kid.clone(),
        fingerprint: fp(&f, &id("a")),
        at: T + 5,
    };
    forge(&mut f, &again);
    let e = f.a.state().entry(&id("a")).unwrap();
    assert_eq!((e.replays, e.consumed_at), (1, Some(T + 3)));
}

#[test]
fn a_denied_request_cannot_be_consumed() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide_mid(&mut f, &id("a"), Choice::Deny, T + 1).unwrap();
    let kid = f.kid.clone();
    assert_eq!(
        refusal(consume(&mut f, &id("a"), &kid, T + 2)),
        Refusal::NotAllowed
    );
    assert_eq!(status(&f, &id("a"), T + 2), Status::Denied);
}

#[test]
fn no_answer_is_denied() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let end = f.a.state().entry(&id("a")).unwrap().req.expires_at;
    assert_eq!(
        status(&f, &id("a"), end - 1),
        Status::Pending { level: None }
    );
    assert_eq!(
        status(&f, &id("a"), end),
        Status::Expired(ExpireReason::Timeout)
    );
    let e = decide(&mut f, &id("a"), By::Owner, Choice::Allow, end);
    assert_eq!(refusal(e), Refusal::Expired, "a late decision is refused");
    // An allow nobody consumed in time is no allow.
    ask(&mut f, "b", T + 10).unwrap();
    decide(&mut f, &id("b"), By::Owner, Choice::Allow, T + 11).unwrap();
    let end = f.a.state().entry(&id("b")).unwrap().req.expires_at;
    assert_eq!(
        status(&f, &id("b"), end),
        Status::Expired(ExpireReason::Timeout)
    );
    let kid = f.kid.clone();
    assert_eq!(
        refusal(consume(&mut f, &id("b"), &kid, end)),
        Refusal::Expired
    );
    // A denial stays a denial.
    ask(&mut f, "c", T + 20).unwrap();
    decide(&mut f, &id("c"), By::Owner, Choice::Deny, T + 21).unwrap();
    assert_eq!(status(&f, &id("c"), T + 5000), Status::Denied);
}

#[test]
fn the_fold_refuses_a_decision_at_the_expiry_and_after_an_expire_record() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let end = f.a.state().entry(&id("a")).unwrap().req.expires_at;
    // Written straight into the file, at the very second the request ends.
    let late = decide_rec(&f, &id("a"), By::Owner, Choice::Allow, Via::Cli, end);
    forge(&mut f, &late);
    assert!(f.a.state().entry(&id("a")).unwrap().decision.is_none());
    // After an expire record, an earlier-dated decision is refused as well.
    ask(&mut f, "b", T + 1).unwrap();
    let kid = f.kid.clone();
    f.d.settle(&kid, Outcome::Failed, Spend::default(), None, None, T + 2)
        .unwrap();
    let ex = Record::Expire {
        req: id("b"),
        reason: ExpireReason::NodeSettled,
        at: T + 3,
    };
    forge(&mut f, &ex);
    let early = decide_rec(&f, &id("b"), By::Owner, Choice::Allow, Via::Cli, T + 4);
    forge(&mut f, &early);
    assert!(f.a.state().entry(&id("b")).unwrap().decision.is_none());
}

#[test]
fn an_expire_record_cannot_undo_a_denial() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide(&mut f, &id("a"), By::Owner, Choice::Deny, T + 1).unwrap();
    let end = f.a.state().entry(&id("a")).unwrap().req.expires_at;
    let ex = Record::Expire {
        req: id("a"),
        reason: ExpireReason::Timeout,
        at: end,
    };
    assert_eq!(refusal(f.a.append(&f.d, ex)), Refusal::Settled);
}

#[test]
fn an_expire_record_only_holds_when_its_reason_does() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let ex = |reason, at| Record::Expire {
        req: id("a"),
        reason,
        at,
    };
    let early = ex(ExpireReason::Timeout, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, early)), Refusal::NoExpiry);
    let running = ex(ExpireReason::NodeSettled, T + 5);
    assert_eq!(refusal(f.a.append(&f.d, running)), Refusal::NoExpiry);
    let end = f.a.state().entry(&id("a")).unwrap().req.expires_at;
    f.a.append(&f.d, ex(ExpireReason::Timeout, end)).unwrap();
    assert_eq!(
        status(&f, &id("a"), T + 5),
        Status::Expired(ExpireReason::Timeout)
    );
}

#[test]
fn a_settled_node_orphans_its_request() {
    let mut f = std_fx();
    let kid = f.kid.clone();
    // One request consumed before the node ends stays consumed.
    ask(&mut f, "z", T).unwrap();
    decide(&mut f, &id("z"), By::Owner, Choice::Allow, T + 1).unwrap();
    consume(&mut f, &id("z"), &kid, T + 2).unwrap();
    ask(&mut f, "a", T + 3).unwrap();
    decide_mid(&mut f, &id("a"), Choice::Allow, T + 4).unwrap();
    ask(&mut f, "d", T + 3).unwrap();
    decide_mid(&mut f, &id("d"), Choice::Deny, T + 4).unwrap();
    f.d.settle(
        &kid,
        Outcome::Failed,
        Spend::default(),
        None,
        Some("x"),
        T + 50,
    )
    .unwrap();
    f.a.refresh(&f.d).unwrap();
    let node_settled = Status::Expired(ExpireReason::NodeSettled);
    assert_eq!(status(&f, &id("a"), T + 51), node_settled);
    assert_eq!(status(&f, &id("z"), T + 51), Status::Consumed);
    assert_eq!(status(&f, &id("d"), T + 51), Status::Denied);
    // The writer refuses to consume or decide for a settled node.
    assert_eq!(
        refusal(consume(&mut f, &id("a"), &kid, T + 51)),
        Refusal::Expired
    );
    let e = decide(&mut f, &id("a"), By::Owner, Choice::Allow, T + 51);
    assert_eq!(refusal(e), Refusal::Expired);
    let ex = Record::Expire {
        req: id("a"),
        reason: ExpireReason::Orphaned,
        at: T + 51,
    };
    f.a.append(&f.d, ex).unwrap();
}

#[test]
fn a_crash_between_decide_and_consume_leaves_no_standing_allow() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide_mid(&mut f, &id("a"), Choice::Allow, T + 1).unwrap();
    // The child restarts (a new fold of the same file): still one allow.
    let mut again = Approvals::open(f.store.clone(), &f.d).unwrap();
    assert_eq!(
        again.state().status(&id("a"), &f.d, T + 2),
        Some(Status::Allowed)
    );
    // The node dies: the allow is gone and cannot be consumed.
    let kid = f.kid.clone();
    f.d.settle(&kid, Outcome::Failed, Spend::default(), None, None, T + 3)
        .unwrap();
    again.refresh(&f.d).unwrap();
    assert_eq!(
        again.state().status(&id("a"), &f.d, T + 4),
        Some(Status::Expired(ExpireReason::NodeSettled))
    );
    let rec = Record::Consume {
        req: id("a"),
        node: kid,
        fingerprint: fp(&f, &id("a")),
        at: T + 4,
    };
    let e = again.append(&f.d, rec).unwrap_err();
    assert!(matches!(e, ApprovalError::Refused(Refusal::Expired)), "{e}");
}

#[test]
fn a_crash_after_consume_never_runs_the_call_twice() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide_mid(&mut f, &id("a"), Choice::Allow, T + 1).unwrap();
    let kid = f.kid.clone();
    consume(&mut f, &id("a"), &kid, T + 2).unwrap();
    // Crash before the tool ran; the restarted child cannot consume again.
    let mut again = Approvals::open(f.store.clone(), &f.d).unwrap();
    assert_eq!(
        again.state().status(&id("a"), &f.d, T + 3),
        Some(Status::Consumed)
    );
    let rec = Record::Consume {
        req: id("a"),
        node: kid,
        fingerprint: fp(&f, &id("a")),
        at: T + 3,
    };
    let e = again.append(&f.d, rec).unwrap_err();
    assert!(
        matches!(e, ApprovalError::Refused(Refusal::AlreadyConsumed)),
        "{e}"
    );
}

#[test]
fn a_torn_tail_is_never_a_decision_and_is_repaired_by_the_writer() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    let rec = decide_rec(&f, &id("a"), By::Owner, Choice::Allow, Via::Cli, T + 1);
    let line = rec.to_line();
    let half = &line.as_bytes()[..line.len() - 5];
    f.store.append(half).unwrap();
    let fresh = Approvals::open(f.store.clone(), &f.d).unwrap();
    assert!(fresh.state().entry(&id("a")).unwrap().decision.is_none());
    assert_eq!(fresh.state().rejected, 0);
    // The next append truncates the half line first.
    ask(&mut f, "b", T + 2).unwrap();
    let bytes = String::from_utf8(f.store.bytes()).unwrap();
    assert!(bytes.ends_with('\n') && !bytes.contains(&line[..line.len() - 5]));
    assert!(f.a.state().entry(&id("a")).unwrap().decision.is_none());
    assert!(f.a.state().entry(&id("b")).is_some());
}

#[test]
fn records_out_of_order_do_nothing() {
    let mut f = std_fx();
    // A decision and a consume written before their request exist.
    let r = req_at(&f, "a", T, |_| {});
    let fpr = r.fingerprint.clone();
    for rec in [
        Record::Decide {
            req: id("a"),
            by: By::Owner,
            verdict: Choice::Allow,
            via: Via::Cli,
            fingerprint: fpr.clone(),
            at: T + 1,
        },
        Record::Consume {
            req: id("a"),
            node: f.kid.clone(),
            fingerprint: fpr.clone(),
            at: T + 2,
        },
    ] {
        forge(&mut f, &rec);
    }
    assert_eq!(f.a.state().rejected, 2);
    f.a.append(&f.d, Record::Request(r)).unwrap();
    let e = f.a.state().entry(&id("a")).unwrap();
    assert!(e.decision.is_none() && e.consumed_at.is_none());
    // A record dated before its request.
    let early = decide_rec(&f, &id("a"), By::Owner, Choice::Allow, Via::Cli, T - 1);
    assert_eq!(refusal(f.a.append(&f.d, early)), Refusal::OutOfOrder);
    // A consume that precedes the decision it relies on.
    decide(&mut f, &id("a"), By::Owner, Choice::Allow, T + 10).unwrap();
    let kid = f.kid.clone();
    assert_eq!(
        refusal(consume(&mut f, &id("a"), &kid, T + 5)),
        Refusal::NotAllowed
    );
}

#[test]
fn a_forged_request_is_ignored() {
    let mut f = std_fx();
    let kid_chain = |f: &Fx| req_at(f, "x", T, |_| {}).chain;
    let good = kid_chain(&f);
    let deadline = f.d.poll(&f.kid).unwrap().budget.deadline;
    type M = Box<dyn Fn(&mut Request)>;
    let g = good.clone();
    let cases: Vec<(&str, M)> = vec![
        (
            "a level marked skipped",
            Box::new({
                let mut c = g.clone();
                c[0].skipped = Some(Skip::OwnerOnly);
                move |r| r.chain = c.clone()
            }),
        ),
        (
            "a level dropped",
            Box::new({
                let c = g[1..].to_vec();
                move |r| r.chain = c.clone()
            }),
        ),
        (
            "levels reordered",
            Box::new({
                let c: Vec<Level> = g.iter().rev().cloned().collect();
                move |r| r.chain = c.clone()
            }),
        ),
        (
            "the requester added",
            Box::new({
                let mut c = g.clone();
                c.insert(
                    0,
                    Level {
                        node: "n9".into(),
                        depth: 9,
                        skipped: None,
                    },
                );
                move |r| r.chain = c.clone()
            }),
        ),
        (
            "an always-owner request with eligible levels",
            Box::new({
                let c = g.clone();
                move |r| {
                    r.secrets = true;
                    r.chain = c.clone();
                }
            }),
        ),
        (
            "fingerprint not hex",
            Box::new(|r| r.fingerprint = "zz".into()),
        ),
        (
            "fingerprint wrong length",
            Box::new(|r| r.fingerprint = "ab".repeat(31)),
        ),
        ("the root asking", Box::new(|r| r.node = "g".into())),
        ("an unknown node", Box::new(|r| r.node = "ghost".into())),
        (
            "a Destructive request",
            Box::new(|r| r.risk = Risk::Destructive),
        ),
        ("a ReadOnly request", Box::new(|r| r.risk = Risk::ReadOnly)),
        ("another goal", Box::new(|r| r.goal = "h".into())),
        ("another session", Box::new(|r| r.session = "sess-x".into())),
        (
            "a NUL in the tool",
            Box::new(|r| r.tool = "write\0file".into()),
        ),
        (
            "a NUL in the target",
            Box::new(|r| r.target = Some("src/a\0.rs".into())),
        ),
        (
            "a window past the node deadline",
            Box::new(move |r| {
                r.at = deadline - 10;
                r.expires_at = deadline + 10;
            }),
        ),
        ("an empty display", Box::new(|r| r.display.clear())),
        ("a bad id", Box::new(|r| r.req = "dap-A B".into())),
        ("a foreign id", Box::new(|r| r.req = "req-1".into())),
        ("no window", Box::new(|r| r.level_secs = 0)),
        (
            "a level window that overflows",
            Box::new(|r| r.level_secs = i64::MAX),
        ),
        (
            "a level window over the cap",
            Box::new(|r| r.level_secs = MAX_APPROVAL_SECS + 1),
        ),
        (
            "a start that overflows the span",
            Box::new(|r| r.at = i64::MIN),
        ),
        ("an empty window", Box::new(|r| r.expires_at = r.at)),
        (
            "a window over the cap",
            Box::new(|r| r.expires_at = r.at + MAX_APPROVAL_SECS + 1),
        ),
        (
            "an oversized stored input",
            Box::new(|r| r.input = json!("x".repeat(MAX_INPUT_BYTES + 1))),
        ),
        (
            "an oversized display",
            Box::new(|r| r.display = "x".repeat(MAX_DISPLAY_BYTES + 1)),
        ),
    ];
    for (i, (name, mutate)) in cases.iter().enumerate() {
        let mut r = req_at(&f, &format!("c{i}"), T, |r| {
            r.fingerprint = "a".repeat(64);
            mutate(r);
        });
        if *name == "the root asking" {
            r.chain.clear(); // the root has no ancestors, so this chain matches
        }
        let before = f.a.state().rejected;
        forge(&mut f, &Record::Request(r.clone()));
        assert!(f.a.state().entry(&r.req).is_none(), "{name} was accepted");
        assert_eq!(f.a.state().rejected, before + 1, "{name}");
    }
}

#[test]
fn a_replayed_log_changes_nothing() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    decide_mid(&mut f, &id("a"), Choice::Allow, T + 1).unwrap();
    let kid = f.kid.clone();
    consume(&mut f, &id("a"), &kid, T + 2).unwrap();
    let log = f.store.bytes();
    f.store.append(&log).unwrap();
    f.a.refresh(&f.d).unwrap();
    // The request id is already used, so the replayed request is ignored and
    // the replayed decision and consume find the request already settled.
    assert_eq!(status(&f, &id("a"), T + 3), Status::Consumed);
    let e = f.a.state().entry(&id("a")).unwrap();
    assert_eq!((e.race_lost, e.replays, f.a.state().rejected), (1, 1, 3));
}

#[test]
fn a_fingerprint_that_does_not_match_the_call_cannot_be_consumed() {
    let mut f = std_fx();
    // The request carries the fingerprint of a different call.
    let other = fingerprint(
        &f.kid,
        "sess-kid",
        "write_file",
        &json!({"p": 1}),
        None,
        "/w",
    );
    let r = req_at(&f, "a", T, |r| r.fingerprint = other.clone());
    f.a.append(&f.d, Record::Request(r)).unwrap();
    decide(&mut f, &id("a"), By::Owner, Choice::Allow, T + 1).unwrap();
    // The child consumes with the fingerprint of the call it is about to run.
    let real = fingerprint(
        &f.kid,
        "sess-kid",
        "write_file",
        &input(),
        Some("src/a.rs"),
        "/w",
    );
    let rec = Record::Consume {
        req: id("a"),
        node: f.kid.clone(),
        fingerprint: real,
        at: T + 2,
    };
    assert_eq!(refusal(f.a.append(&f.d, rec)), Refusal::Fingerprint);
}

#[test]
fn the_record_and_the_log_are_size_capped() {
    let mut f = std_fx();
    ask(&mut f, "a", T).unwrap();
    // A line over the record cap is skipped, whatever it says; the next
    // record still folds.
    let pad = "x".repeat(MAX_RECORD_BYTES);
    let big = Record::Request(req_at(&f, "b", T, |r| r.display = pad.clone()));
    forge(&mut f, &big);
    assert!(f.a.state().entry(&id("b")).is_none());
    assert_eq!(f.a.state().rejected, 1);
    decide(&mut f, &id("a"), By::Owner, Choice::Deny, T + 1).unwrap();
    // Even a request that is otherwise valid: only the line cap stops it.
    let long = Record::Request(req_at(&f, "c", T, |r| {
        r.target = Some("t".repeat(MAX_RECORD_BYTES));
    }));
    assert!(matches!(long, Record::Request(ref r) if r.chain.len() == 2));
    forge(&mut f, &long);
    assert!(f.a.state().entry(&id("c")).is_none());
    // The writer refuses a record over the cap too.
    let mut f2 = std_fx();
    let wide = Record::Request(req_at(&f2, "w", T, |r| {
        r.input = json!({"k": "y".repeat(MAX_INPUT_BYTES - 20)});
        r.display = "d".repeat(MAX_DISPLAY_BYTES);
        r.target = Some("t".repeat(MAX_RECORD_BYTES));
    }));
    let e = f2.a.append(&f2.d, wide).unwrap_err();
    assert!(matches!(e, ApprovalError::RecordTooLarge), "{e}");
    // Past MAX_LOG_BYTES new requests are refused, decisions still land.
    let filler = format!("{}\n", " ".repeat(MAX_RECORD_BYTES - 1));
    let mut ask_ok = std_fx();
    ask(&mut ask_ok, "a", T).unwrap();
    for _ in 0..MAX_LOG_BYTES as usize / filler.len() {
        ask_ok.store.append(filler.as_bytes()).unwrap();
    }
    ask_ok.a.refresh(&ask_ok.d).unwrap();
    let e = ask(&mut ask_ok, "b", T + 1).unwrap_err();
    assert!(matches!(e, ApprovalError::LogFull), "{e}");
    decide(&mut ask_ok, &id("a"), By::Owner, Choice::Deny, T + 2).unwrap();
    // The fold stops reading at the hard cap: a decision past it is not read.
    let mut flood = std_fx();
    ask(&mut flood, "a", T).unwrap();
    for _ in 0..(MAX_LOG_BYTES + (1 << 20)) as usize / filler.len() {
        flood.store.append(filler.as_bytes()).unwrap();
    }
    let late = decide_rec(&flood, &id("a"), By::Owner, Choice::Allow, Via::Cli, T + 1);
    forge(&mut flood, &late);
    assert!(flood.a.state().overflow);
    assert!(flood.a.state().entry(&id("a")).unwrap().decision.is_none());
    let e = decide(&mut flood, &id("a"), By::Owner, Choice::Allow, T + 2).unwrap_err();
    assert!(matches!(e, ApprovalError::LogFull), "{e}");
}

#[test]
fn the_switch_defaults_to_off() {
    assert_eq!(ApprovalsMode::default(), ApprovalsMode::Off);
    let p = |v: &str| serde_json::from_value::<ApprovalsMode>(json!(v));
    assert_eq!(p("off").unwrap(), ApprovalsMode::Off);
    assert_eq!(p("owner").unwrap(), ApprovalsMode::Owner);
    assert_eq!(p("chain").unwrap(), ApprovalsMode::Chain);
    assert!(p("agent").is_err());
}
