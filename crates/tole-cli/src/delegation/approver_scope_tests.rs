//! The approver of a discussion participant that its owner allowed some
//! connected tools (#1039): one approval per tool per discussion.

use super::*;

const SEARCH: &str = "mcp_nginjen_search";
const SCRAPE: &str = "mcp_nginjen_scrape";

/// The approver of a participant of discussion `d-1`, allowed `tools`, with
/// `granted` already approved (what the host found in the ledger).
fn scoped(f: &Fx, tools: &str, granted: &str) -> Option<DelegatedApprover> {
    // What the host launches a participant with: the report tool's pattern.
    scoped_with(f, tools, granted, f.params(&["delegate_report"], false))
}

fn scoped_with(f: &Fx, tools: &str, granted: &str, p: Params) -> Option<DelegatedApprover> {
    let mut env = f.env("1", true);
    env.insert("TOLE_DISCUSSION_ID".into(), "d-1".into());
    env.insert("TOLE_DISCUSSION_TOOLS".into(), tools.into());
    env.insert("TOLE_DISCUSSION_GRANTED".into(), granted.into());
    let src = format!("[delegation]\napprovals = \"owner\"\n{SECS}\n");
    let cfg = crate::config::parse(&src, Path::new("u.toml")).unwrap();
    let get = move |k: &str| env.get(k).cloned();
    for_child(&get, cfg.delegation.as_ref(), p)
}

fn mcp<'a>(tool: &'a str, input: &'a Value) -> ToolRequest<'a> {
    ToolRequest {
        tool,
        risk: Risk::Write,
        input,
        description: format!("{tool} query"),
        target: None,
        summary: None,
    }
}

/// The owner answers request number `n` (0 based).
fn answer(f: &Fx, n: usize, v: Choice, now: i64) {
    let e = f.entries().remove(n);
    let rec = Record::Decide {
        req: e.req.req.clone(),
        by: By::Owner,
        verdict: v,
        via: Via::Cli,
        fingerprint: e.req.fingerprint,
        at: now,
    };
    f.store().write(|d, a| a.append(d, rec)).unwrap();
}

#[test]
fn only_a_participant_with_a_well_formed_allowance_gets_the_scoped_approver() {
    let f = fx("scope-switch", 3600);
    assert!(scoped(&f, SEARCH, "").is_some());
    // No tool, or a name that is not a connected tool's: still the read-only
    // participant with no approver at all.
    assert!(scoped(&f, "", "").is_none());
    assert!(scoped(&f, "write_file,run_command", "").is_none());
    // A granted tool that is not allowed is dropped, not honoured.
    let a = scoped(&f, SEARCH, SCRAPE).unwrap();
    assert!(!a
        .scope
        .as_ref()
        .unwrap()
        .granted
        .lock()
        .unwrap()
        .contains(SCRAPE));
    assert!(!a.interactive(), "Destructive tools stay unregistered");
}

#[test]
fn the_first_call_asks_the_owner_and_later_calls_of_that_tool_do_not() {
    let f = fx("scope-once", 3600);
    f.on_nap({
        let f2 = f.clone();
        move |n, now| {
            if n == 2 {
                answer(&f2, 0, Choice::Allow, now);
            }
        }
    });
    let a = scoped(&f, &format!("{SEARCH},{SCRAPE}"), "").unwrap();
    let i = json!({"q": "invoice tools"});
    assert_eq!(a.decide(&mcp(SEARCH, &i)), Verdict::Allow);
    assert_eq!(f.naps.load(SeqCst), 2, "parked until the answer");
    // The owner saw whose call it is and what allow covers; the request is
    // owner-only (a connected tool is an external effect).
    let e = f.entries().remove(0);
    assert!(
        e.req.display.contains("discussion `d-1`")
            && e.req
                .display
                .contains("every call of this tool in this discussion"),
        "{}",
        e.req.display
    );
    assert!(tole_core::delegation_approvals::owner_only_class(&e.req).is_some());
    // The same tool again, with other input: no new request, no wait.
    assert_eq!(
        a.decide(&mcp(SEARCH, &json!({"q": "other"}))),
        Verdict::Allow
    );
    assert_eq!(f.entries().len(), 1);
    assert_eq!(f.naps.load(SeqCst), 2);
    // Another named tool is a different decision: it asks, and unanswered it
    // is denied.
    f.on_nap(|_, _| {});
    assert_eq!(a.decide(&mcp(SCRAPE, &i)), Verdict::Deny);
    assert_eq!(f.entries().len(), 2);
}

#[test]
fn another_discussion_or_process_asks_again() {
    let f = fx("scope-other", 3600);
    f.on_nap({
        let f2 = f.clone();
        move |n, now| {
            if n == 1 {
                answer(&f2, 0, Choice::Allow, now);
            }
        }
    });
    let i = json!({"q": "x"});
    assert_eq!(
        scoped(&f, SEARCH, "").unwrap().decide(&mcp(SEARCH, &i)),
        Verdict::Allow
    );
    // A new approver (another child, discussion or session) starts with what
    // its host granted it, which here is nothing: it asks.
    f.on_nap(|_, _| {});
    assert_eq!(
        scoped(&f, SEARCH, "").unwrap().decide(&mcp(SEARCH, &i)),
        Verdict::Deny
    );
    assert_eq!(f.entries().len(), 2);
}

#[test]
fn a_tool_the_host_found_granted_runs_without_a_request() {
    let f = fx("scope-granted", 3600);
    let a = scoped(&f, SEARCH, SEARCH).unwrap();
    assert_eq!(a.decide(&mcp(SEARCH, &json!({"q": "x"}))), Verdict::Allow);
    assert!(!approvals_path(&f.log).exists(), "nothing was asked");
    assert_eq!(f.naps.load(SeqCst), 0);
}

#[test]
fn denial_and_no_answer_stop_the_call_and_grant_nothing() {
    let f = fx("scope-deny", 3600);
    f.on_nap({
        let f2 = f.clone();
        move |n, now| {
            if n == 1 {
                answer(&f2, 0, Choice::Deny, now);
            }
        }
    });
    let a = scoped(&f, SEARCH, "").unwrap();
    let i = json!({"q": "x"});
    assert_eq!(a.decide(&mcp(SEARCH, &i)), Verdict::Deny);
    // Nothing is remembered: the next call asks again, and with nobody
    // answering the window ends in a denial and an expiry record.
    f.on_nap(|_, _| {});
    assert_eq!(a.decide(&mcp(SEARCH, &i)), Verdict::Deny);
    let es = f.entries();
    assert_eq!(es.len(), 2);
    assert!(es[0].consumed_at.is_none() && es[1].consumed_at.is_none());
    assert_eq!(es[1].expired.map(|x| x.0), Some(ExpireReason::Timeout));
}

/// The helper must still be able to hand in its contribution: its report
/// tool is Write, allowed by the one pattern its host launched it with.
#[test]
fn the_report_tool_still_works_and_only_by_its_own_pattern() {
    let f = fx("scope-report", 3600);
    let i = json!({"summary": "x"});
    let a = scoped(&f, SEARCH, "").unwrap();
    assert_eq!(a.decide(&mcp("delegate_report", &i)), Verdict::Allow);
    // Without that pattern it is denied, like any tool that was not named.
    let none = scoped_with(&f, SEARCH, "", f.params(&[], true)).unwrap();
    assert_eq!(none.decide(&mcp("delegate_report", &i)), Verdict::Deny);
    assert!(!approvals_path(&f.log).exists(), "none of it was asked");
    assert_eq!(f.naps.load(SeqCst), 0);
}

#[test]
fn nothing_else_is_ever_asked_or_allowed() {
    let f = fx("scope-else", 3600);
    // Even a `*` pattern and `--yes` (a participant has neither) read only
    // for the report tool: they cannot widen it.
    let a = scoped_with(&f, SEARCH, "", f.params(&["*"], true)).unwrap();
    let i = json!({"path": "src/a.rs", "content": "x"});
    for tool in ["write_file", "run_command", "mcp_other_tool", "delegate"] {
        assert_eq!(a.decide(&mcp(tool, &i)), Verdict::Deny, "{tool}");
    }
    // Destructive is denied even when named and granted.
    let d = ToolRequest {
        tool: SEARCH,
        risk: Risk::Destructive,
        input: &i,
        description: "x".into(),
        target: None,
        summary: None,
    };
    assert_eq!(
        scoped(&f, SEARCH, SEARCH).unwrap().decide(&d),
        Verdict::Deny
    );
    // ReadOnly tools run, as they always did for a participant.
    let ro = ToolRequest {
        tool: "read_file",
        risk: Risk::ReadOnly,
        input: &i,
        description: "read".into(),
        target: None,
        summary: None,
    };
    assert_eq!(a.decide(&ro), Verdict::Allow);
    assert!(!approvals_path(&f.log).exists(), "none of it was asked");
    assert_eq!(f.naps.load(SeqCst), 0);
}

#[test]
fn a_deny_pattern_is_final_for_a_named_tool_even_once_the_owner_allowed_it() {
    let f = fx("scope-deny", 3600);
    let i = json!({"q": "tole"});
    // The deny pattern of the allow list wins over naming the tool, over a
    // grant found in the ledger and over a request that was never asked.
    let p = f.params(&["delegate_report", "!mcp_nginjen_*"], false);
    let a = scoped_with(&f, SEARCH, SEARCH, p).unwrap();
    assert_eq!(a.decide(&mcp(SEARCH, &i)), Verdict::Deny);
    let p = f.params(&["delegate_report", "!mcp_nginjen_*"], false);
    let b = scoped_with(&f, SEARCH, "", p).unwrap();
    assert_eq!(b.decide(&mcp(SEARCH, &i)), Verdict::Deny);
    assert!(!approvals_path(&f.log).exists(), "nothing was asked");
    assert_eq!(f.naps.load(SeqCst), 0);
    // A deny of another tool does not touch this one: it still asks.
    let p = f.params(&["delegate_report", "!mcp_other_*"], false);
    let c = scoped_with(&f, SEARCH, SEARCH, p).unwrap();
    assert_eq!(c.decide(&mcp(SEARCH, &i)), Verdict::Allow);
}

/// What the discussion driver reads back from the queue (#1039): an allow
/// (once consumed too), a denial and an unanswered request, for this helper's
/// node only, and nothing while a request is still waiting.
#[test]
fn the_driver_reads_the_owners_answers_back_per_helper() {
    use crate::discuss::helper_tools::{decisions, ToolDecision};
    use tole_core::discussion::ToolVerdict;
    const THIRD: &str = "mcp_nginjen_third";
    let f = fx("scope-decisions", 3600);
    let seen = |f: &Fx, now: i64| {
        let mut d = decisions(&f.store(), &f.kid, now).unwrap();
        d.sort_by(|a, b| a.tool.cmp(&b.tool));
        d.into_iter()
            .map(|x: ToolDecision| (x.tool, x.verdict))
            .collect::<Vec<_>>()
    };
    f.on_nap({
        let f2 = f.clone();
        move |n, now| match n {
            1 => answer(&f2, 0, Choice::Allow, now),
            2 => answer(&f2, 1, Choice::Deny, now),
            // The third request is waiting: it is not listed yet.
            3 => assert_eq!(seen(&f2, now).len(), 2),
            _ => {}
        }
    });
    let a = scoped(&f, &format!("{SEARCH},{SCRAPE},{THIRD}"), "").unwrap();
    let i = json!({"q": "x"});
    assert_eq!(a.decide(&mcp(SEARCH, &i)), Verdict::Allow);
    assert_eq!(a.decide(&mcp(SCRAPE, &i)), Verdict::Deny);
    f.on_nap({
        let f2 = f.clone();
        move |n, now| {
            if n == 1 {
                assert_eq!(seen(&f2, now).len(), 2, "waiting is not listed");
            }
        }
    });
    assert_eq!(a.decide(&mcp(THIRD, &i)), Verdict::Deny);
    let now = f.clock.load(SeqCst);
    assert_eq!(
        seen(&f, now),
        [
            (SCRAPE.to_string(), ToolVerdict::Deny),
            (SEARCH.to_string(), ToolVerdict::Allow),
            (THIRD.to_string(), ToolVerdict::Expired),
        ]
    );
    // Another node's requests are not this helper's.
    assert!(decisions(&f.store(), "somebody-else", now)
        .unwrap()
        .is_empty());
}
