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
    assert!(start(&many, &env(&dir, &on, make)).is_err());
    let bad = StartArgs {
        tools: vec!["write_file".into()],
        ..args(1_000_000)
    };
    let (_, make) = factory(10, agreeable());
    assert!(start(&bad, &env(&dir, &on, make)).is_err());
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
