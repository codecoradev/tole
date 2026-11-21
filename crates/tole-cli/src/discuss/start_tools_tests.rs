//! `tole discuss` with an allowance of helper tools (#1039): what is checked
//! before anything is written or called.

use super::*;

const SEARCH: &str = "mcp_nginjen_search";

fn cfg_with(approvals: &str) -> Config {
    let src = format!(
        "[profiles.coder]\n[profiles.ops]\n[profiles.scribe.models]\nlight = \"tiny\"\n{}",
        if approvals.is_empty() {
            String::new()
        } else {
            format!("[delegation]\napprovals = \"{approvals}\"\n")
        }
    );
    crate::config::parse(&src, Path::new("user.toml")).unwrap()
}

fn tool_args() -> StartArgs {
    StartArgs {
        tools: vec![SEARCH.into()],
        ..args(1_000_000)
    }
}

#[test]
fn the_dry_run_shows_the_allowance_and_needs_the_approvals_queue() {
    let dir = tmp("dtools-plan");
    let (_, make) = factory(10, agreeable());
    let off = cfg_with("");
    let e = plan(&tool_args(), &env(&dir, &off, make.clone())).unwrap_err();
    assert!(e.contains("approvals = \"owner\""), "{e}");
    let on = cfg_with("owner");
    let text = plan(&tool_args(), &env(&dir, &on, make)).unwrap();
    assert!(
        text.contains(&format!("Helper tools: {SEARCH}"))
            && text.contains("independent read-only child")
            && text.contains("approvals queue"),
        "{text}"
    );
    assert!(
        !dir.join("pick-db.jsonl").exists(),
        "a dry run writes nothing"
    );
    // Without tools the plan says nothing about them.
    let (_, make) = factory(10, agreeable());
    let plain = plan(&args(1_000_000), &env(&dir, &on, make)).unwrap();
    assert!(!plain.contains("Helper tools"), "{plain}");
}

#[test]
fn a_start_that_cannot_ask_the_owner_or_start_a_helper_writes_nothing() {
    let dir = tmp("dtools-start");
    // Approvals off: every call would be denied, so it is refused.
    let (calls, make) = factory(10, agreeable());
    let off = cfg_with("off");
    let e = start(&tool_args(), &env(&dir, &off, make)).unwrap_err();
    assert!(e.contains("approvals = \"owner\""), "{e}");
    assert!(!dir.join("pick-db.jsonl").exists());
    // Approvals on but no host to start children.
    let (_, make) = factory(10, agreeable());
    let on = cfg_with("owner");
    let e = start(&tool_args(), &env(&dir, &on, make)).unwrap_err();
    assert!(e.contains("no delegation host"), "{e}");
    assert!(!dir.join("pick-db.jsonl").exists());
    assert_eq!(calls.lock().unwrap().len(), 0, "no model was called");
    // More than the allowed number of tools, or a name no tool has.
    let many = StartArgs {
        tools: (0..9).map(|i| format!("mcp_s_t{i}")).collect(),
        ..args(1_000_000)
    };
    let (_, make) = factory(10, agreeable());
    let e = start(&many, &env(&dir, &on, make)).unwrap_err();
    assert!(e.contains("at most 8"), "{e}");
    let bad = StartArgs {
        tools: vec!["write_file".into()],
        ..args(1_000_000)
    };
    let (_, make) = factory(10, agreeable());
    let e = start(&bad, &env(&dir, &on, make)).unwrap_err();
    assert!(e.contains("not a connected tool name"), "{e}");
    assert!(!dir.join("pick-db.jsonl").exists());
}

#[test]
fn a_budget_that_cannot_fit_the_helper_children_is_flagged() {
    let c = Contract::new(
        "pick-db",
        "q",
        &["c".to_string()],
        &["coder".to_string(), "ops".to_string()],
        30_000,
        None,
        &SecretSet::default(),
    )
    .unwrap();
    assert!(advice(&c)
        .iter()
        .all(|l| !l.contains("independent children")));
    let with = c.with_tools(&[SEARCH.to_string()]).unwrap();
    let a = advice(&with);
    assert!(
        a.iter()
            .any(|l| l.contains("independent children") && l.contains("40000")),
        "{a:?}"
    );
}

/// A ledger of a discussion started with tools, as `tole discuss start` left
/// it: open, nothing said yet.
fn open_tools_ledger(dir: &Path) -> DiscussionLog {
    let c = Contract::new(
        "pick-db",
        "Which database?",
        &["a choice is named".to_string()],
        &["coder".to_string(), "ops".to_string()],
        1_000_000,
        Some(3),
        &SecretSet::default(),
    )
    .unwrap()
    .with_tools(&[SEARCH.to_string()])
    .unwrap();
    let log = DiscussionLog::at(dir, "pick-db").unwrap();
    log.start(c, 100).unwrap();
    log
}

/// A helper is seated only when the next step can start one: a stop, a close
/// or a look at a finished discussion needs no delegation host, so it works
/// from `tole serve` (no host) and after `[delegation] approvals` was turned
/// off.
#[test]
fn a_stopped_or_finished_tools_discussion_closes_and_shows_without_a_delegation_host() {
    let dir = tmp("dtools-stopped");
    let log = open_tools_ledger(&dir);
    // Still open and about to need a helper: refused, as before.
    let on = cfg_with("owner");
    let (_, make) = factory(10, agreeable());
    let e = resume("pick-db", &env(&dir, &on, make)).err().unwrap();
    assert!(e.contains("no delegation host"), "{e}");
    // The owner stops it from a host without delegation (serve's Env has no
    // delegator, and approvals may be off): the ledger closes.
    let off = cfg_with("");
    let (calls, make) = factory(10, agreeable());
    let out = owner::Owner::default()
        .stop("pick-db", &env(&dir, &off, make))
        .unwrap();
    assert_eq!(out["state"], "closed", "{out}");
    assert!(out.get("note").is_none(), "{out}");
    assert!(log.state().unwrap().closed.is_some());
    // A finished discussion prints its outcome again, whatever the config.
    let (_, make) = factory(10, agreeable());
    let text = resume("pick-db", &env(&dir, &off, make)).unwrap();
    assert!(text.contains("Outcome: "), "{text}");
    assert_eq!(calls.lock().unwrap().len(), 0, "no model was called");
}

/// The same for a stop recorded in a ledger nobody closed yet.
#[test]
fn a_recorded_stop_is_closed_by_resume_without_a_helper() {
    let dir = tmp("dtools-stop-recorded");
    let log = open_tools_ledger(&dir);
    log.stop(200).unwrap();
    assert!(log.state().unwrap().closed.is_none());
    let off = cfg_with("");
    let (_, make) = factory(10, agreeable());
    let text = resume("pick-db", &env(&dir, &off, make)).unwrap();
    assert!(text.contains("Outcome: "), "{text}");
    assert!(log.state().unwrap().closed.is_some());
}
